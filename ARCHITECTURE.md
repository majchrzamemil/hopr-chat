# hopr-chat architecture

hopr-chat is a peer-to-peer messenger whose transport is the HOPR mixnet. Every install runs its own
HOPR node (hoprd) **inside the app process**. The chat itself is a thin protocol on top of HOPR
sessions: signed JSON lines plus acknowledgements, with a SQLite outbox for retries.

```
                ┌──────────────────────── one process ─────────────────────────┐
  frontend      │  TUI (ratatui)          │  GUI (Tauri: Svelte UI)            │
                │          all implement the `Ui` trait (src/app.rs)           │
                ├──────────────────────────────────────────────────────────────┤
  flows         │  app.rs: production (onboarding → node → chat) / dev / attach│
                ├──────────────────────────────────────────────────────────────┤
  chat core     │  chat.rs: sessions, wire protocol, acks, retries, signing    │
                │  db.rs:   SQLite chat.db (contacts, messages, outbox)        │
                ├─────────────── REST + TCP on 127.0.0.1 only ─────────────────┤
  node          │  hoprd (library), own thread + tokio runtime                 │
                │  → hopr-lib: p2p (QUIC), mixing, sessions, tickets, Safe     │
                └───────────────┬──────────────────────────────┬───────────────┘
                                │ HTTPS                        │ QUIC/UDP
                        blokli (chain indexer)          HOPR relays → peer's node
```

## Repository layout

| Path | What |
|---|---|
| `src/lib.rs` | library root; everything below `src/` except the frontends is UI-free |
| `src/app.rs` | `Ui` trait, `SafeChoice`/`Status`, the production/dev/attach flows |
| `src/onboard.rs` | identity, Safe lookup/creation, writes `hoprd.yaml` |
| `src/settings.rs` | `settings.toml`, app home location, network list |
| `src/hoprd.rs` | hoprd REST client, `embed()` (in-process node), `launch()` (child process), localcluster helpers |
| `src/chat.rs` | chat core: `Chat`, `Event`, wire protocol, signing, wallet |
| `src/db.rs` | SQLite schema and queries |
| `src/tui.rs`, `src/term.rs` | terminal frontend, plus the stdout onboarding that runs before it |
| `src/main.rs` | clap entry point for the terminal binary |
| `app/src-tauri/` | Tauri crate: `TauriUi`, commands, Android init |
| `app/src/` | Svelte 5 web UI (onboarding, chat, modals) |
| `app/src-tauri/gen/android/` | Android project: Gradle, manifest, `MainActivity`, `NodeService` |

## Frontends and the `Ui` trait

Every frontend implements the same three methods; the flows in `app.rs` are generic over it:

```rust
pub trait Ui {
    async fn choose_safe(&mut self, node_address: &str) -> Result<SafeChoice>;   // first start only
    fn status(&mut self, status: Status);                                       // onboarding progress
    async fn run(&mut self, chat: Arc<Chat>, events: UnboundedReceiver<Event>) -> Result<()>;  // main loop
}
```

- **TUI** (`TuiUi`) renders in the terminal; onboarding prompts run on plain stdout before it starts.
- **GUI** (`TauriUi`, `app/src-tauri/src/lib.rs`) keeps a `Snapshot` of statuses so a reloaded webview
  can catch up, forwards `Event`s to the webview as `chat_event`, and exposes the core as Tauri
  commands (`conversations`, `history`, `send`, `add_peer`, `peers`, `wallet`, `purge`, `remove_peer`, …).
  The mode (own node, localcluster, attach) is picked from environment variables.

`run` returns when the user quits. The flow then calls `chat.shutdown()` (closes sessions) and stops
hoprd, waiting until it is torn down, so the process can exit right after.

## Startup flow (production mode)

1. **Settings**: `settings.toml` in the app home: nick, `blokli_url` (network), hops, ports.
2. **Single instance**: the API port is test-bound. If it is busy, another frontend already runs this
   node and startup stops (TUI and GUI share the home).
3. **Identity** (`onboard::identity`): `hopr.id` (encrypted keystore, created on first start) plus a
   random `hopr.pass`. It holds the chain key (the node's address, which also signs chat messages) and
   the packet key.
4. **Safe** (`onboard::safe`): `hopr.safe` if present. Otherwise edgli asks blokli whether a Safe owned
   by this node exists. Otherwise the UI asks:
   - *existing*: the user enters a Safe and module that already include this node (e.g. made with hopli
     for several nodes).
   - *create*: wait for xDai + wxHOPR on the node address, then `deploy_safe`. The node key becomes the
     Safe owner.
5. **hoprd config**: `hoprd.yaml` is rewritten every start. It contains the identity, Safe/module,
   blokli URL, p2p host, the API on 127.0.0.1, and a session allow list with only this app's own chat
   target.
6. **Node**: `hoprd::embed()` (below). On first start hopr-lib registers the node with the Safe and
   announces it on chain.
7. **Chat**: `chat::start()` binds the chat listener, spawns the accept and retry loops, and hands
   `Chat` + an event receiver to `ui.run()`.

`dev` mode attaches to a localcluster node (identity read from the cluster's config); a nick keeps
the node it got first, so its address is stable across runs. `attach` mode
uses any running hoprd over its REST API.

## Embedded hoprd

`src/hoprd.rs::embed()` (cargo feature `embedded`, on by default; `--no-default-features` falls back
to spawning a hoprd binary with `launch()`):

```
embed(keys, hoprd.yaml, api_port, hoprd.log)
  ├─ random API token → written to hoprd.token (0600) so the user can curl the node
  ├─ tracing subscriber → hoprd.log (on Android: logcat, set up earlier)
  ├─ hoprd::cli::CliArgs::try_parse_from(["hoprd", "--configurationFilePath", …, "--api", …])
  │    → hoprd::config::HoprdConfig      (exactly what the hoprd binary would build)
  ├─ thread "hoprd": hopr_lib::prepare_tokio_runtime() → block_on(select!{
  │        hoprd::main_inner(cfg, keys),   // the real daemon
  │        stop_rx })                      // the returned `stop` closes it and joins the thread
  └─ poll /readyz until ready (fails early with the log tail if the thread exits)
```

- hoprd gets its own runtime because it expects large worker stacks (10 MB). It also keeps a busy node
  from starving the UI runtime.
- The core still talks to it over **REST on 127.0.0.1** with the token, the same API a standalone
  hoprd offers. Embedded and sidecar modes therefore share one code path, and you can inspect the node
  with `curl`.
- Version pinning: hopr-chat pins **edgli** (onboarding: Safe lookup/deploy, balances) and **hoprd**
  (the node) to git revs. Both are hopr-lib 5. edgli tracks hoprnet `master` and hoprd pins a
  rev, so a `[patch]` in `Cargo.toml` (via a `www.github.com` URL, because cargo rejects a patch
  pointing at its own source) forces one hopr-lib. That is required: `HoprKeys` from onboarding is
  passed straight into `main_inner`.

## Chat over HOPR

**Addressing.** A contact is just a node address (`0x…`). Each node's chat listener sits on a
deterministic local port, `127.0.0.1:(20000 + FNV-1a(address) % 40000)` (`chat::chat_target`). The
sender therefore knows the session target from the address alone. hoprd's allow list only permits that
one target.

**Sending** (`Chat::send` → `flush`):

1. The message is stored in `chat.db` as `pending` (outbox), id = current time in nanoseconds.
2. Without an open session, `POST /api/v4/session/tcp` asks the local node for a session to the
   peer's node: `forwardPath`/`returnPath` = `{Hops: n}` (default 1), target = the peer's chat port.
   hoprd returns a local TCP socket; bytes written there travel through the mixnet.
3. The core writes one JSON line per message and marks it `sent`.
4. The peer's node delivers the stream to its chat listener. That app stores the message and writes an
   `ack` line back, which returns over the same session via SURBs (single-use reply blocks, so the
   receiver never learns a route to the sender). The sender marks the message `delivered`.

**Wire protocol** (newline-delimited JSON on the session stream):

```json
{"t":"msg","id":42,"from":"0xsender","nick":"alice","text":"hi","sig":"<hex>"}
{"t":"ack","id":42}
```

**Signatures.** `sig` is the node chain key's signature over
`hopr-chat/v2|<recipient address, lowercase>|<id>|<nick byte length>:<nick>|<text>`. The receiver
recovers the signer and compares it with `from` and with itself as recipient: verified, forged, or
unsigned. The length prefix stops a nick/text split from being shifted; the tag keeps these signatures
apart from ticket and transaction signatures.

**Filing.** Anyone can open a session to the chat port, and `from` and `nick` are only claims. So:

- a forged signature is dropped;
- a message claiming a contact's address is filed under that contact only if verified, else dropped;
- everything else (strangers) is filed under the lowercase sender address, at most 100 unread each;
- the UIs label messages with the contact's saved nick or the address, never with the claimed nick.

Lines are capped at 16 KiB (longer ones close the connection), text at 4000 characters, nicks at 32,
and control characters are stripped before anything reaches a terminal. Rejected messages are still
acked so their sender stops resending.

**Reliability.** The retry loop (every 15 s) closes sessions whose sent messages got no ack within 30 s
(back to `pending`), and redials every contact with pending messages, each in its own task. Sessions
are opened without holding the session map, so one unreachable peer stalls nobody else. On restart `sent` becomes
`pending` again, since sessions do not survive restarts. Duplicates are dropped by `(dir, peer, id)`.

**Unknown senders.** A message from an address that is not a contact shows up under that address and
is answered over its live incoming connection (SURBs) while it lasts; after that, replies are queued and
dialed to that address like a contact's. Adding the address as a contact moves the conversation under
the new nick.

## What HOPR provides (and what it costs)

- **Sessions**: hopr-lib sends the stream as SPHINX packets over `hops` relays (1 by default; 0 is
  direct and not anonymous). Each relay only knows its neighbours.
- **Payment**: every relayed packet carries a ticket drawn on a **payment channel** from the sender to
  the first relay. The channel strategy opens channels from the Safe to a few relays and sizes them by
  traffic: 10 MiB per channel (`write_hoprd_config`; hoprd's default is 1 GiB, sized for VPN). On
  piz-palu-prod a ticket costs 1e-5 wxHOPR at win probability ~4e-6, so the stake hits its floor of one
  3-hop winning ticket, 7.5 wxHOPR per channel (1 GiB would be ~30).
- **Funds**: the **Safe** holds wxHOPR and gives the Channels contract an allowance. The **node
  address** only holds xDai for gas. Registering a node with a Safe costs 0.01 wxHOPR.
- **Reachability**: nodes behind NAT announce no address. Peers and relays reach them over connections
  the node opened itself.

How to check that traffic really goes through relays (channels, connected peers, tcpdump): see the
README, "Verify traffic really goes over HOPR".

## Storage

The app home holds everything: `~/Library/Application Support/hopr-chat/` (macOS),
`~/.local/share/hopr-chat/` (Linux), `/data/user/0/org.hoprnet.chat/` (Android).

| File | Owner | Content |
|---|---|---|
| `hopr.id`, `hopr.pass` | onboarding | node identity + its password (back up together) |
| `hopr.safe` | onboarding | Safe + module address |
| `settings.toml` | app | nick, network, hops, ports |
| `hoprd.yaml`, `hoprd.token`, `hoprd.log`, `hoprd-db/` | node | config (regenerated), API token, log, node DB |
| `chat.db` | chat core | SQLite, below |

`chat.db` tables:

- `peers(nick PK, address, last_seen, last_attempt, last_err)`: contacts.
- `messages(dir, peer, id, text, status, attempts, created, updated, from_addr, nick, verified, conn, read)`,
  primary key `(dir, peer, id)`. `dir` is `in`/`out`. Outgoing `status` is
  `pending → sent → delivered`. `peer` is a contact nick, or a raw address for unknown senders.

Schema changes are additive (`ALTER TABLE … ADD COLUMN`, errors ignored), so old databases open fine.

**Deleting.** `purge` (one conversation or all), and `remove_peer` (contact + its messages), run with
`PRAGMA secure_delete=ON` (freed pages are zeroed) followed by `wal_checkpoint(TRUNCATE)`. The text is
then gone from both the database and its WAL file, which a test checks byte-for-byte.

## Platforms

### Desktop

- **Terminal binary** (`cargo build --release`): the TUI.
- **GUI** (`app/`, Tauri 2 + Svelte 5): `npm run tauri dev`, or `npm run bundle` for an .app/.dmg.
- Both embed hoprd and share the app home: one node, one identity, run one at a time.

### Android

The same Tauri app built with `npx tauri android build --apk -t aarch64` (NDK r28+, which also gives the
16 KB page alignment Android 15 requires). hoprd builds for `aarch64-linux-android` unmodified.
Making it *run* inside an app took the following, all in `android_init()` and the Gradle project:

| Problem | Fix |
|---|---|
| hickory DNS reads the system resolver through JNI and panics without an Android context | tao (Tauri's windowing) initialises `ndk_context`; nothing to do |
| reqwest/rustls verifies TLS with Android's trust store via JNI | `rustls_platform_verifier::android::init_with_env` with the activity, plus its Kotlin half (`org.rustls:rustls-platform-verifier`, version must equal `rustls-platform-verifier-android` in `Cargo.lock`) and a ProGuard keep rule |
| hoprd's ticket store and chain cache use temp files; the default temp dir is not writable | `TMPDIR` = app cache dir |
| app data must live in the sandbox | `HOPR_CHAT_HOME` = app data dir |
| logs | tracing to stderr, which Tauri forwards to logcat |
| hoprd installs SIGINT/SIGTERM handlers | harmless in-process; shutdown is by dropping the node |
| the global rayon pool can be set only once | the node starts once per process |
| Android kills background apps | `NodeService`: a foreground service (`specialUse`) with an ongoing notification keeps the process alive; not sticky, and it ends when the app is swiped away |

- **Signing**: release APKs are signed with the key named in `gen/android/key.properties`
  (gitignored; without it the APK is unsigned, see the README). Updates keep the app data only when
  signed with the same key.
- **UI**: below 700 px the GUI shows either the chat list or one conversation, Android back returns to
  the list, and `env(safe-area-inset-*)` keeps content out from under the system bars.

## Networks

`settings::NETWORKS` maps a name to a blokli URL. The only entry is **piz-palu-prod** (HOPR v5, Gnosis
Chain), which matches hopr-lib 5; older networks speak another protocol. A changed blokli URL wipes
`hoprd-db`; the Safe must already be scoped for the new network's contracts.

## Known limits

- Phones run a full node; running one that does not relay for others would need a hoprd option.
- The ticket store is a temp file, so a relaying node loses unredeemed tickets on restart.
- Nodes announced from behind NAT have no public address. Inbound sessions rely on connections the
  node keeps open.
- There is no mailbox: messages wait in the sender's outbox until both sides are online.
- A signed message reveals the sender's node address to the recipient (by design).
