# hopr-chat

Anonymous P2P chat over HOPR sessions. The app runs its own hoprd node (inside the same process), signs
every message with the node's chain key, and keeps conversations in SQLite. Frontends: TUI (default),
line-mode CLI, desktop GUI (Tauri) and Android (same Tauri app).

## Where things live

Every frontend in production mode uses one **app home**. TUI and desktop GUI on the same machine share it,
so they are the same node with the same chats.

| Platform | App home |
|---|---|
| macOS | `~/Library/Application Support/hopr-chat/` |
| Linux | `$XDG_DATA_HOME/hopr-chat/` or `~/.local/share/hopr-chat/` |
| Android | `/data/user/0/org.hoprnet.chat/` (app-private; cache in `…/cache/`) |
| any | `HOPR_CHAT_HOME=<dir>` (or `--home` for the TUI) overrides it |

| File | What | Lose it and… |
|---|---|---|
| `hopr.id` | node identity: chain key (node address, signs messages) + packet key, encrypted keystore | the node is gone; a new identity is a new node |
| `hopr.pass` | random password for `hopr.id` (0600). Anyone with both files has the node key | `hopr.id` can't be opened |
| `hopr.safe` | Safe + module address (YAML) | re-enter them ("Use an existing Safe") |
| `settings.toml` | nick, network (`blokli_url`), hops, API/p2p ports | defaults come back |
| `chat.db` | peers, messages, delivery state (SQLite) | chat history |
| `hoprd.yaml` | hoprd config, rewritten on every start | nothing |
| `hoprd-db/` | hoprd's own database | resynced from chain |
| `hoprd.log` | hoprd + core log (desktop/TUI; Android logs to logcat) | nothing |
| `prompt-history.txt` | CLI line history | nothing |

**Back up `hopr.id` + `hopr.pass` together** (or the whole home). On desktop they survive app updates and
rebuilds. On Android they survive updates only if the new APK is signed with the same key (see Android
below); uninstall or "Clear storage" deletes them. If the app created the Safe itself ("Create a new
Safe"), the node key is the Safe owner, so losing it also loses the Safe's funds. A Safe owned by a
separate (Ledger) address avoids that.

Other locations: dev mode (localcluster) uses `~/.hopr-chat/<nick>/` (TUI) and the cluster's own
`/tmp/hopr-nodes/`; the Android signing key is `~/.android/hopr-chat-release.jks`.

## Desktop: TUI or GUI

Both run the same node from the same app home. **Only one at a time**: the second one refuses to start
(`port 3100 is busy: is hopr-chat (TUI or desktop app) already running?`). Quit one, start the other;
identity, Safe and chats carry over.

    cargo run --release                 # TUI (repo root); add --cli with --features cli
    cd app && npm run tauri dev         # GUI (dev build)
    cd app && npm run bundle            # GUI release bundle (.app/.dmg); then open it like any app

## One Safe for several nodes (phone + desktop)

The in-app "Create a new Safe" makes one Safe per node, owned by that node's key. To share one Safe
between your nodes, create it with hopli (`hoprnet/hopli`, supports `piz-palu-dev`):

    PRIVATE_KEY=<gas-only deployer key> hopli safe-module create \
      --network piz-palu-dev --provider-url https://rpc.gnosischain.com \
      --node-address <node1>,<node2> \
      --admin-address <owner> --threshold 1 --allowance 100

- `--admin-address` is the Safe owner: the only key that can withdraw funds or add/remove nodes. Use a
  dedicated (ideally Ledger) account. Omit it and the deployer key becomes the owner.
- Node addresses: the app shows them on the Safe screen, or
  `hopli identity read --identity-from-path <home>/hopr.id --password-path <home>/hopr.pass`.
- Then fund on Gnosis Chain: wxHOPR (`0xD4fd…A2c1`) to the Safe, ~0.05 xDai gas to each node.
- In each app choose "Use an existing Safe" and enter the Safe + module addresses (saved as `hopr.safe`).

## Layout

    src/lib.rs        library: everything below is UI-free
      app.rs          `Ui` trait + production / dev / attach flows
      onboard.rs      first start: identity, Safe (existing or deployed), hoprd.yaml
      chat.rs         core: sessions, acks, retries, signing, wallet; emits `Event`s
      db.rs           SQLite: peers, messages (nick, verified, unread), conversations
      hoprd.rs        hoprd REST client, in-process `embed()` or child-process `launch()`, localcluster
      settings.rs     versioned settings.toml
    src/tui.rs        `TuiUi: Ui` (ratatui) — default frontend
    src/cli.rs        `CliUi: Ui` (rustyline) — only with `--features cli`, picked with `--cli`
    src/term.rs       onboarding prompts/status shared by both     src/main.rs  clap
    app/              Tauri desktop GUI (src-tauri: `TauriUi: Ui` + commands, src: Svelte UI)

A GUI implements the same `Ui` trait (`choose_safe`, `status`, `run`) and calls the same `app::*` flows.

## Frontends

    cargo build                      # TUI only (hoprd embedded)
    cargo build --features cli       # TUI + line-mode CLI; run with --cli
    cargo build --no-default-features   # spawn an external hoprd binary instead of embedding it

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

Other env vars: `HOPR_CHAT_HOPS`, `HOPR_CHAT_NETWORK=piz-palu|jura|rotsee`, `HOPR_CHAT_HOME`,
`HOPR_CHAT_DEV_NODE`, `HOPR_CHAT_CLUSTER_DIR`. The GUI's dev modes (`HOPR_CHAT_DEV_NICK`) keep data in
`~/.hopr-chat/<nick>/`.

**hoprd runs inside the app** (`embedded` feature, on by default) on desktop and Android, so there is no
sidecar to build or bundle. Its log is `hoprd.log` in the app home.

## Android app

Same Tauri app; hoprd runs **inside the app process** via `hoprd::main_inner`, on its own runtime.
The chat core still talks to it over REST on `127.0.0.1`. Network: piz-palu (hopr-lib 5).

Prerequisites: Android SDK (`ANDROID_HOME`), NDK r28+ (`NDK_HOME`, 16 KB pages), JDK 17,
`rustup target add aarch64-linux-android`. On this machine:

    export ANDROID_HOME=/opt/homebrew/share/android-commandlinetools
    export NDK_HOME=$ANDROID_HOME/ndk/28.2.13676358 JAVA_HOME=/opt/homebrew/opt/openjdk@17

Build and install (phone with USB debugging on, `adb devices` lists it):

    cd app
    npx tauri android build --apk -t aarch64             # ~10 min
    adb install -r src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release.apk
    adb logcat | grep RustStdoutStderr                   # core + hoprd logs

**Signing = keeping your identity.** `adb install -r` upgrades in place and keeps the app data only when
the APK is signed with the same key. The release signing key is `~/.android/hopr-chat-release.jks`; its
passwords are in `app/src-tauri/gen/android/key.properties` (gitignored). Back both up. With another key
the install fails, and the only way out is uninstalling, which deletes the identity.

Android specifics:
- `android_init` in `app/src-tauri/src/lib.rs` points `HOPR_CHAT_HOME` and `TMPDIR` (hoprd's ticket
  store) into the app sandbox. It also initialises `rustls-platform-verifier` with the JVM so blokli's
  TLS uses Android's trust store. Its Kotlin half is a Gradle dependency (`org.rustls:rustls-platform-verifier`),
  and that version must match `rustls-platform-verifier-android` in `Cargo.lock`.
- `NodeService` is a foreground service with an ongoing notification. It keeps the process, and so the
  node, alive in the background.
- Narrow screens show either the chat list or one conversation; back returns to the list.

**UI features**
- Onboarding screens: identity, Safe choice (existing or create), funding with a live balance, hoprd startup.
- Chat list with unread counts and search (⌘K). New chat (⌘N) checks that the address is an announced node.
- Message bubbles with sender nick, signature badge (✓ verified / unsigned / ✗ forged?) and delivery state
  (🕓 queued, ✓ sent, ✓✓ delivered).
- Balance chip with gas/funds warnings, a share-address dialog, and toasts.

**Shutdown.** Closing the window shuts down cleanly: sessions are closed and the node is stopped.

## Production (own node)

    hopr-chat --network piz-palu --nick alice    # first start; later just `hopr-chat`

The first start (TUI or GUI) does the following:

1. Creates the node identity (`hopr.id` + generated `hopr.pass`) in the app home (see "Where things live").
2. Finds the node's Safe:
   - If a Safe owned by this node already exists on chain, it uses that.
   - Otherwise it asks you:
     - **existing**: enter the Safe and module addresses. The node must already be added to that module,
       e.g. via the HOPR staking hub.
     - **create**: fund the shown node address with xDai + wxHOPR. The app waits, then deploys the Safe
       and moves the wxHOPR into it, the same way as GnosisVPN.
3. Writes `hoprd.yaml` and starts hoprd in-process. On first start hoprd registers the node with the Safe
   (0.01 wxHOPR from the Safe, gas from the node) and announces it on chain.
   - The allow list contains only this chat's target (`127.0.0.1:<port derived from the address>`).
   - The API is on 127.0.0.1 with a random token.
   - hoprd's default strategies auto-open channels.

Only for `--no-default-features` builds, an external hoprd is found in this order: `hoprd_bin` in
settings, `HOPR_CHAT_HOPRD_BIN`, the binary next to hopr-chat, `PATH`. Settings (`nick`, `blokli_url`, `hops`, ports) live in `settings.toml`;
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

Desktop: `cd app && npm run bundle` (one app, hoprd inside). Android: the APK above. Not done yet: a
signed update manifest, store builds, and a faucet hook for test funds.

## Limits

- `--hops 0` is direct and not anonymous.
- A signed message reveals your node address to the receiver.
- There is no mailbox: both sides must be online at some point.
- An existing Safe must already include this node in its module. Otherwise hoprd fails at startup;
  check `hoprd.log`.
