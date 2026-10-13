use std::path::Path;

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct Msg {
    pub id: i64,
    pub peer: String,
    pub dir: String,
    pub text: String,
    pub status: String,
    pub created: i64,
    /// Sender's self-chosen nick (incoming only).
    pub nick: Option<String>,
    /// Claimed sender node address (incoming only).
    pub from_addr: Option<String>,
    /// `verified` | `unsigned` | `invalid` (incoming only).
    pub verified: Option<String>,
    /// Inbound connection to `/reply` on (incoming only).
    pub conn: Option<u32>,
}

#[derive(Serialize, Clone)]
pub struct Peer {
    pub nick: String,
    pub address: String,
    pub last_seen: Option<i64>,
    pub last_attempt: Option<i64>,
    pub last_err: Option<String>,
    /// Filled by the chat core: an outgoing session is open.
    pub online: bool,
}

#[derive(Serialize, Clone)]
pub struct Conversation {
    pub peer: String,
    pub last_text: String,
    pub last_at: i64,
    pub unread: i64,
}

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

pub fn open(path: &Path) -> Result<Connection> {
    let db = Connection::open(path)?;
    db.pragma_update(None, "journal_mode", "WAL")?;
    init(&db)?;
    Ok(db)
}

pub fn init(db: &Connection) -> Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS peers (
            nick TEXT PRIMARY KEY, address TEXT NOT NULL,
            last_seen INTEGER, last_attempt INTEGER, last_err TEXT);
         CREATE TABLE IF NOT EXISTS messages (
            id INTEGER NOT NULL, peer TEXT NOT NULL, dir TEXT NOT NULL, text TEXT NOT NULL,
            status TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
            created INTEGER NOT NULL, updated INTEGER NOT NULL, from_addr TEXT,
            PRIMARY KEY (dir, peer, id));",
    )?;
    // older DBs: errors mean the column already exists / is already gone
    for col in ["nick TEXT", "verified TEXT", "conn INTEGER", "read INTEGER NOT NULL DEFAULT 0"] {
        let _ = db.execute(&format!("ALTER TABLE messages ADD COLUMN {col}"), []);
    }
    let _ = db.execute("ALTER TABLE peers DROP COLUMN target", []);
    // sessions don't survive a restart, so anything in flight must be resent
    db.execute("UPDATE messages SET status = 'pending' WHERE dir = 'out' AND status = 'sent'", [])?;
    Ok(())
}

pub fn add_peer(db: &Connection, nick: &str, address: &str) -> Result<()> {
    db.execute(
        "INSERT INTO peers (nick, address) VALUES (?1, ?2)
         ON CONFLICT(nick) DO UPDATE SET address = ?2",
        params![nick, address],
    )?;
    // messages from/to this node stored before it had a nick
    db.execute(
        "UPDATE OR IGNORE messages SET peer = ?1 WHERE lower(peer) = lower(?2)",
        params![nick, address],
    )?;
    Ok(())
}

pub fn peers(db: &Connection) -> Result<Vec<Peer>> {
    let mut stmt = db.prepare(
        "SELECT nick, address, last_seen, last_attempt, last_err FROM peers ORDER BY nick",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Peer {
            nick: r.get(0)?,
            address: r.get(1)?,
            last_seen: r.get(2)?,
            last_attempt: r.get(3)?,
            last_err: r.get(4)?,
            online: false,
        })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn peer_address(db: &Connection, nick: &str) -> Result<Option<String>> {
    Ok(db
        .query_row("SELECT address FROM peers WHERE nick = ?1", [nick], |r| r.get(0))
        .optional()?)
}

pub fn nick_by_address(db: &Connection, address: &str) -> Result<Option<String>> {
    Ok(db
        .query_row(
            "SELECT nick FROM peers WHERE lower(address) = lower(?1)",
            [address],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn peer_attempt(db: &Connection, nick: &str, err: Option<String>) -> Result<()> {
    db.execute(
        "UPDATE peers SET last_attempt = ?2, last_err = ?3 WHERE nick = ?1",
        params![nick, now(), err],
    )?;
    Ok(())
}

pub fn peer_seen(db: &Connection, nick: &str) -> Result<()> {
    db.execute(
        "UPDATE peers SET last_seen = ?2, last_err = NULL WHERE nick = ?1",
        params![nick, now()],
    )?;
    Ok(())
}

pub fn insert_out(db: &Connection, peer: &str, id: i64, text: &str, status: &str) -> Result<()> {
    db.execute(
        "INSERT INTO messages (id, peer, dir, text, status, created, updated, read)
         VALUES (?1, ?2, 'out', ?3, ?4, ?5, ?5, 1)",
        params![id, peer, text, status, now()],
    )?;
    Ok(())
}

/// Returns false when the message was already stored (redelivery after a lost ack).
pub fn insert_in(db: &Connection, m: &Msg) -> Result<bool> {
    let n = db.execute(
        "INSERT OR IGNORE INTO messages
            (id, peer, dir, text, status, created, updated, from_addr, nick, verified, conn)
         VALUES (?1, ?2, 'in', ?3, 'received', ?4, ?4, ?5, ?6, ?7, ?8)",
        params![m.id, m.peer, m.text, m.created, m.from_addr, m.nick, m.verified, m.conn],
    )?;
    Ok(n > 0)
}

pub fn pending(db: &Connection, peer: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = db.prepare(
        "SELECT id, text FROM messages WHERE dir = 'out' AND peer = ?1 AND status = 'pending'
         ORDER BY created, id",
    )?;
    let rows = stmt.query_map([peer], |r| Ok((r.get(0)?, r.get(1)?)))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

pub fn mark_sent(db: &Connection, peer: &str, id: i64) -> Result<()> {
    db.execute(
        "UPDATE messages SET status = 'sent', attempts = attempts + 1, updated = ?3
         WHERE dir = 'out' AND peer = ?1 AND id = ?2",
        params![peer, id, now()],
    )?;
    Ok(())
}

/// Returns true only on the first ack for a message.
pub fn mark_delivered(db: &Connection, peer: &str, id: i64) -> Result<bool> {
    let n = db.execute(
        "UPDATE messages SET status = 'delivered', updated = ?3
         WHERE dir = 'out' AND peer = ?1 AND id = ?2 AND status != 'delivered'",
        params![peer, id, now()],
    )?;
    Ok(n > 0)
}

pub fn requeue(db: &Connection, peer: &str) -> Result<()> {
    db.execute(
        "UPDATE messages SET status = 'pending' WHERE dir = 'out' AND peer = ?1 AND status = 'sent'",
        [peer],
    )?;
    Ok(())
}

pub fn stale_sent(db: &Connection, ack_timeout_secs: i64) -> Result<Vec<String>> {
    let mut stmt = db.prepare(
        "SELECT DISTINCT peer FROM messages WHERE dir = 'out' AND status = 'sent' AND updated < ?1",
    )?;
    let rows = stmt.query_map([now() - ack_timeout_secs], |r| r.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

/// Only known peers: replies to never-added senders stay pending until `/add`.
pub fn peers_with_pending(db: &Connection) -> Result<Vec<String>> {
    let mut stmt = db.prepare(
        "SELECT DISTINCT m.peer FROM messages m JOIN peers p ON p.nick = m.peer
         WHERE m.dir = 'out' AND m.status = 'pending'",
    )?;
    let rows = stmt.query_map([], |r| r.get(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

const MSG_COLS: &str = "id, peer, dir, text, status, created, nick, from_addr, verified, conn";

fn msg_row(r: &Row) -> rusqlite::Result<Msg> {
    Ok(Msg {
        id: r.get(0)?,
        peer: r.get(1)?,
        dir: r.get(2)?,
        text: r.get(3)?,
        status: r.get(4)?,
        created: r.get(5)?,
        nick: r.get(6)?,
        from_addr: r.get(7)?,
        verified: r.get(8)?,
        conn: r.get(9)?,
    })
}

/// Runs a newest-first query and returns the rows oldest-first, ready to print.
fn msgs(db: &Connection, filter: &str, args: impl rusqlite::Params) -> Result<Vec<Msg>> {
    let sql = format!("SELECT {MSG_COLS} FROM messages {filter}");
    let mut stmt = db.prepare(&sql)?;
    let mut out: Vec<Msg> = stmt.query_map(args, msg_row)?.collect::<Result<_, _>>()?;
    out.reverse();
    Ok(out)
}

/// `address` also matches messages stored before the sender was added as a peer.
pub fn history(db: &Connection, peer: &str, address: Option<&str>, n: i64) -> Result<Vec<Msg>> {
    msgs(
        db,
        "WHERE peer = ?1 OR lower(peer) = lower(?2) ORDER BY created DESC, rowid DESC LIMIT ?3",
        params![peer, address.unwrap_or(""), n],
    )
}

pub fn outbox(db: &Connection) -> Result<Vec<Msg>> {
    msgs(db, "WHERE dir = 'out' AND status IN ('pending', 'sent') ORDER BY created DESC, rowid DESC", [])
}

pub fn mark_read(db: &Connection, peer: &str, address: Option<&str>) -> Result<()> {
    db.execute(
        "UPDATE messages SET read = 1 WHERE dir = 'in' AND (peer = ?1 OR lower(peer) = lower(?2))",
        params![peer, address.unwrap_or("")],
    )?;
    Ok(())
}

pub fn conversations(db: &Connection) -> Result<Vec<Conversation>> {
    let mut stmt = db.prepare(
        "SELECT m.peer, m.text, m.created,
                (SELECT COUNT(*) FROM messages u WHERE u.peer = m.peer AND u.dir = 'in' AND u.read = 0)
         FROM messages m
         WHERE m.rowid = (SELECT x.rowid FROM messages x WHERE x.peer = m.peer
                          ORDER BY x.created DESC, x.rowid DESC LIMIT 1)
         ORDER BY m.created DESC, m.rowid DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok(Conversation { peer: r.get(0)?, last_text: r.get(1)?, last_at: r.get(2)?, unread: r.get(3)? })
    })?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mem() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        init(&db).unwrap();
        add_peer(&db, "b", "0xB").unwrap();
        db
    }

    fn incoming(peer: &str, id: i64, text: &str) -> Msg {
        Msg {
            id,
            peer: peer.into(),
            dir: "in".into(),
            text: text.into(),
            status: "received".into(),
            created: now(),
            nick: Some("bob".into()),
            from_addr: Some(peer.into()),
            verified: Some("verified".into()),
            conn: Some(1),
        }
    }

    fn status(db: &Connection, id: i64) -> String {
        db.query_row("SELECT status FROM messages WHERE id = ?1", [id], |r| r.get(0)).unwrap()
    }

    #[test]
    fn outgoing_lifecycle() {
        let db = mem();
        insert_out(&db, "b", 1, "hi", "pending").unwrap();
        assert_eq!(peers_with_pending(&db).unwrap(), vec!["b"]);
        mark_sent(&db, "b", 1).unwrap();
        assert!(pending(&db, "b").unwrap().is_empty());
        assert!(mark_delivered(&db, "b", 1).unwrap());
        assert!(!mark_delivered(&db, "b", 1).unwrap());
        assert_eq!(status(&db, 1), "delivered");
    }

    #[test]
    fn sent_goes_back_to_pending_on_restart_and_requeue() {
        let db = mem();
        insert_out(&db, "b", 1, "a", "pending").unwrap();
        insert_out(&db, "b", 2, "b", "pending").unwrap();
        mark_sent(&db, "b", 1).unwrap();
        mark_sent(&db, "b", 2).unwrap();
        mark_delivered(&db, "b", 2).unwrap();
        init(&db).unwrap();
        assert_eq!(status(&db, 1), "pending");
        assert_eq!(status(&db, 2), "delivered");
        mark_sent(&db, "b", 1).unwrap();
        requeue(&db, "b").unwrap();
        assert_eq!(pending(&db, "b").unwrap(), vec![(1, "a".to_string())]);
    }

    #[test]
    fn incoming_dedup_keeps_nick_and_verification() {
        let db = mem();
        assert!(insert_in(&db, &incoming("0xb", 7, "yo")).unwrap());
        assert!(!insert_in(&db, &incoming("0xb", 7, "yo")).unwrap());
        insert_out(&db, "b", 8, "hey", "pending").unwrap();
        let h = history(&db, "b", Some("0xB"), 10).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!(h[0].nick.as_deref(), Some("bob"));
        assert_eq!(h[0].verified.as_deref(), Some("verified"));
    }

    #[test]
    fn conversations_group_by_peer_with_unread() {
        let db = mem();
        insert_in(&db, &incoming("b", 1, "one")).unwrap();
        insert_in(&db, &incoming("b", 2, "two")).unwrap();
        insert_out(&db, "c", 3, "to c", "pending").unwrap();
        let convs = conversations(&db).unwrap();
        assert_eq!(convs.len(), 2);
        let b = convs.iter().find(|c| c.peer == "b").unwrap();
        assert_eq!((b.unread, b.last_text.as_str()), (2, "two"));
        mark_read(&db, "b", None).unwrap();
        assert_eq!(conversations(&db).unwrap().iter().find(|c| c.peer == "b").unwrap().unread, 0);
    }

    #[test]
    fn replies_to_unknown_senders_are_not_dialed() {
        let db = mem();
        insert_out(&db, "0xstranger", 1, "hi", "pending").unwrap();
        assert!(peers_with_pending(&db).unwrap().is_empty());
        assert_eq!(outbox(&db).unwrap().len(), 1);
    }

    #[test]
    fn migrates_v1_schema() {
        let db = Connection::open_in_memory().unwrap();
        db.execute_batch(
            "CREATE TABLE peers (nick TEXT PRIMARY KEY, address TEXT NOT NULL, target TEXT NOT NULL,
                last_seen INTEGER, last_attempt INTEGER, last_err TEXT);
             CREATE TABLE messages (id INTEGER NOT NULL, peer TEXT NOT NULL, dir TEXT NOT NULL,
                text TEXT NOT NULL, status TEXT NOT NULL, attempts INTEGER NOT NULL DEFAULT 0,
                created INTEGER NOT NULL, updated INTEGER NOT NULL, from_addr TEXT,
                PRIMARY KEY (dir, peer, id));",
        )
        .unwrap();
        init(&db).unwrap();
        add_peer(&db, "b", "0xB").unwrap();
        insert_in(&db, &incoming("b", 1, "x")).unwrap();
        assert_eq!(conversations(&db).unwrap()[0].unread, 1);
    }
}
