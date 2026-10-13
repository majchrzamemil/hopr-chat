//! UI-agnostic chat core: sessions over hoprd, acks, retries, signing. UIs drive it through the
//! public methods and render the `Event`s it emits.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU32, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::Duration,
};

use anyhow::{bail, Context, Result};
use edgli::{
    blokli::ChainKeypair,
    hopr_lib::{api::types::crypto::prelude::Signature, builder::Keypair},
};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{
        tcp::{OwnedReadHalf, OwnedWriteHalf},
        TcpListener, TcpStream,
    },
    sync::mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
};

use crate::{db, hoprd};

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

fn new_id() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as i64
}

/// Every chat listens on a port derived from its node address, so the address alone is enough to reach it.
pub fn chat_target(address: &str) -> String {
    let hash = address
        .to_lowercase()
        .bytes()
        .fold(0xcbf29ce484222325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    format!("127.0.0.1:{}", 20000 + hash % 40000)
}

/// Binding the recipient and id stops replays to other peers or as new messages; the prefix keeps
/// these signatures apart from the chain key's ticket and transaction signatures.
fn signed_bytes(to: &str, id: i64, nick: &str, text: &str) -> Vec<u8> {
    format!("hopr-chat/v1|{}|{id}|{nick}|{text}", to.to_lowercase()).into_bytes()
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
    outgoing: tokio::sync::Mutex<HashMap<String, (OwnedWriteHalf, String)>>,
    /// conn id -> (writer back over SURBs, peer label, sender address)
    incoming: tokio::sync::Mutex<HashMap<u32, (OwnedWriteHalf, String, String)>>,
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
        self.api.ensure_announced(address).await?;
        db::add_peer(&self.db(), nick, address)
    }

    /// Queues for a known peer; for a sender that was never added, answers on its live connection.
    pub async fn send(self: &Arc<Self>, peer: &str, text: &str) -> Result<()> {
        if db::peer_address(&self.db(), peer)?.is_some() {
            db::insert_out(&self.db(), peer, new_id(), text, "pending")?;
            self.wake(peer);
            return Ok(());
        }
        let conn = self.incoming.lock().await.iter().find(|(_, (_, label, _))| label.eq_ignore_ascii_case(peer)).map(|(c, _)| *c);
        match conn {
            Some(conn) => self.reply(conn, text).await,
            None => bail!("unknown peer {peer}: /add it, or wait until it messages you"),
        }
    }

    pub async fn reply(&self, conn: u32, text: &str) -> Result<()> {
        let id = new_id();
        let mut incoming = self.incoming.lock().await;
        let (w, label, to) = incoming.get_mut(&conn).context("connection gone, use /msg")?;
        let sig = self.key.as_ref().map(|k| sign(k, to, id, &self.nick, text));
        let msg = Wire::Msg { id, from: self.me.clone(), nick: self.nick.clone(), text: text.into(), sig };
        w.write_all(&line(&msg)).await?;
        db::insert_out(&self.db(), label, id, text, "sent")
    }

    pub async fn peers(&self) -> Result<Vec<db::Peer>> {
        let open = self.outgoing.lock().await;
        let mut peers = db::peers(&self.db())?;
        for p in &mut peers {
            p.online = open.contains_key(&p.nick);
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

    /// Inbound connections that `/reply` can answer; empty while the map is busy.
    pub fn open_connections(&self) -> Vec<u32> {
        self.incoming.try_lock().map(|m| m.keys().copied().collect()).unwrap_or_default()
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

    pub fn mark_read(&self, peer: &str) -> Result<()> {
        let db = self.db();
        let address = db::peer_address(&db, peer)?;
        db::mark_read(&db, peer, address.as_deref())
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
        let socks: Vec<String> = self.outgoing.lock().await.drain().map(|(_, (_, s))| s).collect();
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
            if let Err(e) = chat.flush(&peer).await {
                chat.emit(Event::Unreachable { peer, error: format!("{e:#}") });
            }
        });
    }

    async fn flush(self: &Arc<Self>, peer: &str) -> Result<()> {
        let pending = db::pending(&self.db(), peer)?;
        if pending.is_empty() {
            return Ok(());
        }
        let address = db::peer_address(&self.db(), peer)?.with_context(|| format!("unknown peer {peer}"))?;
        let mut out = self.outgoing.lock().await;
        if !out.contains_key(peer) {
            let dialed = self.dial(&address).await;
            db::peer_attempt(&self.db(), peer, dialed.as_ref().err().map(|e| format!("{e:#}")))?;
            let (stream, sock) = dialed?;
            let (r, w) = stream.into_split();
            tokio::spawn(self.clone().read_outgoing(peer.to_string(), r, sock.clone()));
            out.insert(peer.to_string(), (w, sock));
        }
        let (w, _) = out.get_mut(peer).expect("inserted above");
        for (id, text) in pending {
            let sig = self.key.as_ref().map(|k| sign(k, &address, id, &self.nick, &text));
            let msg = Wire::Msg { id, from: self.me.clone(), nick: self.nick.clone(), text, sig };
            if let Err(e) = w.write_all(&line(&msg)).await {
                drop(out);
                self.close_outgoing(peer, None).await;
                return Err(e.into());
            }
            db::mark_sent(&self.db(), peer, id)?;
        }
        Ok(())
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
        let mut out = self.outgoing.lock().await;
        if sock.is_some_and(|s| out.get(peer).is_none_or(|(_, cur)| cur != s)) {
            return;
        }
        let removed = out.remove(peer);
        drop(out);
        if let Some((_, s)) = removed {
            self.api.close_session(&s).await;
        }
        if let Err(e) = db::requeue(&self.db(), peer) {
            self.emit(Event::Warning(format!("db: {e:#}")));
        }
    }

    async fn read_outgoing(self: Arc<Self>, peer: String, r: OwnedReadHalf, sock: String) {
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            match serde_json::from_str::<Wire>(&l) {
                Ok(Wire::Ack { id }) => self.on_ack(&peer, id),
                Ok(Wire::Msg { id, from, nick, text, sig }) => {
                    self.on_msg(None, id, &from, &nick, &text, sig.as_deref());
                    if let Some((w, _)) = self.outgoing.lock().await.get_mut(&peer) {
                        let _ = w.write_all(&line(&Wire::Ack { id })).await;
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
            let Ok((stream, _)) = listener.accept().await else { continue };
            let conn = self.next_conn.fetch_add(1, Ordering::Relaxed);
            let (r, w) = stream.into_split();
            self.incoming.lock().await.insert(conn, (w, String::new(), String::new()));
            tokio::spawn(self.clone().read_incoming(conn, r));
        }
    }

    async fn read_incoming(self: Arc<Self>, conn: u32, r: OwnedReadHalf) {
        let mut lines = BufReader::new(r).lines();
        while let Ok(Some(l)) = lines.next_line().await {
            let mut incoming = self.incoming.lock().await;
            let Some((w, label, sender)) = incoming.get_mut(&conn) else { break };
            match serde_json::from_str::<Wire>(&l) {
                Ok(Wire::Msg { id, from, nick, text, sig }) => {
                    *label = self.on_msg(Some(conn), id, &from, &nick, &text, sig.as_deref());
                    *sender = from;
                    let _ = w.write_all(&line(&Wire::Ack { id })).await;
                }
                Ok(Wire::Ack { id }) => self.on_ack(label, id),
                Err(e) => self.emit(Event::Warning(format!("bad line on #{conn}: {e}"))),
            }
        }
        self.incoming.lock().await.remove(&conn);
    }

    /// Stores and announces an incoming message, returns the peer label it was stored under.
    fn on_msg(&self, conn: Option<u32>, id: i64, from: &str, nick: &str, text: &str, sig: Option<&str>) -> String {
        let db = self.db();
        let peer = db::nick_by_address(&db, from).ok().flatten().unwrap_or_else(|| from.to_string());
        let msg = db::Msg {
            id,
            peer: peer.clone(),
            dir: "in".into(),
            text: text.into(),
            status: "received".into(),
            created: db::now(),
            nick: Some(nick.into()),
            from_addr: Some(from.into()),
            verified: Some(verify(&self.me, from, id, nick, text, sig).into()),
            conn,
        };
        let stored = db::insert_in(&db, &msg);
        drop(db);
        match stored {
            Ok(true) => self.emit(Event::Message(msg)),
            Ok(false) => {}
            Err(e) => self.emit(Event::Warning(format!("db: {e:#}"))),
        }
        peer
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
            let due = db::peers_with_pending(&self.db()).unwrap_or_default();
            for peer in due {
                if let Err(e) = self.flush(&peer).await {
                    self.emit(Event::Unreachable { peer, error: format!("{e:#}") });
                }
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
    }
}
