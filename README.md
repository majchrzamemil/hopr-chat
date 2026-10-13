# hopr-chat

Anonymous P2P chat over HOPR sessions. One binary supervises its own `hoprd`, signs every message with
the node's chain key, and keeps conversations in SQLite.

## Layout

    src/lib.rs        library: everything below is UI-free
      app.rs          `Ui` trait + production / dev / attach flows
      onboard.rs      first start: identity, Safe (existing or deployed), hoprd.yaml
      chat.rs         core: sessions, acks, retries, signing, wallet; emits `Event`s
      db.rs           SQLite: peers, messages (nick, verified, unread), conversations
      hoprd.rs        hoprd REST client, process supervision, localcluster
      settings.rs     versioned settings.toml
    src/tui.rs        `TuiUi: Ui` (ratatui) — default frontend
    src/cli.rs        `CliUi: Ui` (rustyline) — only with `--features cli`, picked with `--cli`
    src/term.rs       onboarding prompts/status shared by both     src/main.rs  clap
    app/              Tauri desktop GUI (src-tauri: `TauriUi: Ui` + commands, src: Svelte UI)

A GUI implements the same `Ui` trait (`choose_safe`, `status`, `run`) and calls the same `app::*` flows.

## Frontends

    cargo build                      # TUI only
    cargo build --features cli       # TUI + line-mode CLI; run with --cli

TUI: `?` help overlay, `/` opens a command palette (Tab/Enter completes, ↑↓ picks), ↑↓ switch chat,
PgUp/PgDn scroll, Ctrl-W/Ctrl-U edit, Esc closes popups, Ctrl-C quits. Outgoing messages show `…` pending, `✓` sent, `✓✓` delivered.

## Desktop app (Tauri)

`app/` is a Tauri v2 GUI (Svelte 5 web UI) over the same library. `app/src-tauri/src/lib.rs` has
`TauriUi: Ui`, plus commands that the web UI calls.

    cd app && npm install
    npm run tauri dev                                    # own node (production flow, onboarding UI)
    HOPR_CHAT_DEV_NICK=a npm run tauri dev               # localcluster node, like `hopr-chat dev`
    HOPR_CHAT_DEV_NICK=b npm run tauri:second            # 2nd window: reuses the dev server on :1420
    HOPR_CHAT_ATTACH_API=http://localhost:3002 HOPR_CHAT_NICK=me \
      HOPR_CHAT_HOPRD_CONFIG=/tmp/hopr-nodes/hoprd_cfg_2.yaml npm run tauri dev   # running hoprd

Other env vars: `HOPR_CHAT_HOPS`, `HOPR_CHAT_NETWORK=rotsee|jura`, `HOPR_CHAT_HOME`, `HOPR_CHAT_HOPRD_BIN`,
`HOPR_CHAT_DEV_NODE`, `HOPR_CHAT_CLUSTER_DIR`.

**Release bundle (.app/.dmg/.deb).** hoprd is bundled next to the app and found through `current_exe()`:

    mkdir -p app/src-tauri/binaries
    cp <path to hoprd> app/src-tauri/binaries/hoprd-$(rustc -vV | sed -n 's/host: //p')
    cd app && npm run bundle

**UI features**
- Onboarding screens: identity, Safe choice (existing or create), funding with a live balance, hoprd startup.
- Chat list with unread counts and search (⌘K). New chat (⌘N) checks that the address is an announced node.
- Message bubbles with sender nick, signature badge (✓ verified / unsigned / ✗ forged?) and delivery state
  (🕓 queued, ✓ sent, ✓✓ delivered).
- Balance chip with gas/funds warnings, a share-address dialog, and toasts.

**Shutdown.** Closing the window shuts down cleanly: sessions are closed and the bundled hoprd is stopped.
During onboarding the setup is aborted, which still kills a hoprd it spawned.

## Production (own node)

    hopr-chat --network rotsee --nick alice      # first start; later just `hopr-chat`

The first start does the following:

1. Creates the node identity (`hopr.id` + generated `hopr.pass`, mode 0600) in the app home.
   The app home is `~/Library/Application Support/hopr-chat` on macOS and `~/.local/share/hopr-chat` on Linux.
   Override it with `HOPR_CHAT_HOME` or `--home`.
2. Finds the node's Safe:
   - If a Safe owned by this node already exists on chain, it uses that.
   - Otherwise it asks you:
     - **existing**: enter the Safe and module addresses. The node must already be added to that module,
       e.g. via the HOPR staking hub.
     - **create**: fund the shown node address with xDai + wxHOPR. The app waits, then deploys the Safe
       and moves the wxHOPR into it, the same way as GnosisVPN.
3. Writes `hoprd.yaml` and starts hoprd. On first start hoprd registers the Safe and announces the node on chain.
   - The allow list contains only this chat's target (`127.0.0.1:<port derived from the address>`).
   - The API is on 127.0.0.1 with a random token.
   - hoprd's default strategies auto-open channels.

hoprd is found in this order: `hoprd_bin` in settings, `HOPR_CHAT_HOPRD_BIN`, the binary **next to
hopr-chat** (release bundle), `PATH`. Settings (`nick`, `blokli_url`, `hops`, ports) live in `settings.toml`;
the `--nick`, `--network`, `--blokli-url` and `--hops` flags update them.

The p2p port (default 9100) must be reachable from the internet so relays can deliver incoming sessions.

## Dev (localcluster)

See the prerequisites below, then:

    cargo run -- dev --nick a     # starts the cluster if needed, attaches to node 0
    cargo run -- dev --nick b     # node 1; node 2 relays (default 1 hop)

Dev mode signs with each node's key, read from `/tmp/hopr-nodes/hoprd_cfg_<id>.yaml`.
`cargo run -- dev-down` stops the cluster.

`attach --api URL --nick n [--hoprd-config cfg.yaml]` uses any running hoprd. Messages are signed only if the
config is given.

### Prerequisites

localcluster runs anvil + blokli in a container:

    orbctl start && docker info
    docker pull --platform linux/amd64 \
      europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil@sha256:75956aede10a71fbb8a46fae05d1aa1caed10ae6ed410c4385df3d565b4fe4fc

The image is amd64-only and runs under Rosetta on Apple silicon; the digest is the hoprd CI pin. Build hoprd and
hoprd-localcluster in the hoprd repo (`nix build .#binary-hoprd`, `.#binary-hoprd-localcluster`) and export
`HOPR_CHAT_HOPRD_BIN` and `HOPR_CHAT_LOCALCLUSTER_BIN`.

Free ports: 8080, 3000-3002, 9000-9002. If the cluster gets stuck:
`dev-down`, `docker rm -f hopr-chain`, `rm -rf /tmp/hopr-nodes`.

## Using it (CLI)

    /add bob 0x...        save a peer (hoprd checks it is an announced node)
    /open bob             focus; plain text now goes to bob, prompt shows a→bob>
    /chats                conversations, unread counts
    /history bob 50       with sender nick and ✓ (verified) / ? (unsigned) / ✗ (forged) badges
    /balance              node + Safe balances
    /reply 3 text         answer inbound connection #3 over SURBs (works without /add)
    /peers /outbox /whoami /close /help /quit

Tab completes commands, peers and connection ids.

- **Delivery:** messages are queued in SQLite and count as delivered only on the peer's ack. Peers with
  pending messages are redialed every 15s, and unacked messages are resent after a restart.
- **Signatures:** each message is signed over `hopr-chat/v1|recipient|id|nick|text` with the node's chain key.
  The receiver recovers the signer address and compares it with the claimed sender.

## Releases

A release is a tarball per platform with `hopr-chat` and `hoprd` side by side. Next steps, following
GnosisVPN's patterns:

- macOS .pkg / app bundle (or a Tauri GUI with hoprd bundled alongside)
- a signed update manifest
- a faucet hook for test funds

## Limits

- `--hops 0` is direct and not anonymous.
- A signed message reveals your node address to the receiver.
- There is no mailbox: both sides must be online at some point.
- An existing Safe must already include this node in its module. Otherwise hoprd fails at startup;
  check `hoprd.log`.
