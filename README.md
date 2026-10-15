# hopr-chat

Anonymous P2P chat over HOPR sessions. The app runs its own hoprd node inside the same process, signs
every message with the node's chain key, and keeps conversations in SQLite. Frontends: a terminal UI
(TUI) and a GUI (Tauri) for desktop and Android.

How the pieces fit together (UI ↔ core ↔ embedded hoprd ↔ HOPR network, wire protocol, storage,
Android specifics) is in [ARCHITECTURE.md](ARCHITECTURE.md).

## Quick start

    cargo run --release                      # TUI; first start walks through onboarding
    cd app && npm install && npm run tauri dev   # GUI (dev build)
    cd app && npm run bundle                 # GUI release bundle (.app/.dmg on macOS)

TUI and GUI on the same machine are the same node with the same chats. **Only one runs at a time**: the
second refuses to start (`port 3100 is busy: is hopr-chat (TUI or desktop app) already running?`).

## First start (own node)

    hopr-chat --nick alice        # later just `hopr-chat`

1. Creates the node identity (`hopr.id` + generated `hopr.pass`) in the app home.
2. Finds the node's Safe:
   - If a Safe owned by this node already exists on chain, it uses that.
   - Otherwise it asks:
     - **existing**: enter the Safe and module addresses. The node must already be added to that
       module, e.g. via the HOPR staking hub.
     - **create**: fund the shown node address with xDai + wxHOPR. The app waits, then deploys the
       Safe and moves the wxHOPR into it.
3. Writes `hoprd.yaml` and starts hoprd in-process. On first start hoprd registers the node with the
   Safe (0.01 wxHOPR from the Safe, gas from the node) and announces it on chain.
   - The session allow list contains only this chat's target (`127.0.0.1:<port derived from the address>`).
   - The REST API listens on 127.0.0.1 with a random token.
   - hoprd's default strategies auto-open channels.

Network is piz-palu-prod (HOPR v5, Gnosis Chain). Settings (`nick`, `blokli_url`, `hops`, ports) live in
`settings.toml`; `--nick`, `--network`, `--blokli-url` and `--hops` update them.

The p2p port (default 9100, UDP) does not need to be reachable from the internet: a node behind NAT
announces no address and is reached through relays over connections it opened itself.

## Using the TUI

`?` help, `/` command palette (Tab/Enter completes, ↑↓ picks), ↑↓ switch chat, PgUp/PgDn scroll,
Ctrl-W/Ctrl-U edit, Esc closes popups, Ctrl-C quits.

    /add bob 0x...     save a contact (hoprd checks it is an announced node)
    /peers             contacts and their reachability
    /balance           node and Safe balances
    /whoami            the /add line to give to others
    /clear [all]       wipe the open chat (or every chat) from disk
    /remove [nick]     delete a contact and its messages (default: open chat)

Outgoing messages show `…` queued, `✓` sent, `✓✓` delivered. Incoming ones show `✓` (signed by the
sender's node) or `?` (unsigned).

- **Delivery:** messages are queued in SQLite and count as delivered only on the peer's ack. Peers with
  queued messages are redialed every 15 s, and unacked messages are resent after a restart.
- **Signatures:** a message from a contact must carry a valid signature from the contact's node,
  otherwise it is dropped. Unsigned messages are accepted only from strangers (not in your contacts),
  and show up under the sender's address. Replies to a stranger go back over its open connection, or,
  once that is gone, to its node address like to a contact. `/remove` on a stranger's chat deletes it.

## GUI

Same core as the TUI (`app/src-tauri` implements the `Ui` trait, `app/src` is the Svelte 5 UI).

- Onboarding screens: identity, Safe choice (existing or create), funding with a live balance, node startup.
- Chat list with unread counts and search (⌘K). New chat (⌘N) checks that the address is an announced node.
- Message bubbles with the contact's name, signature badge and delivery state (🕓 queued, ✓ sent, ✓✓ delivered).
- Balance chip with gas/funds warnings, a share-address dialog, toasts.
- **Clear** (chat header) wipes a conversation, **Delete contact** removes the contact with its
  messages, **Delete all messages** (sidebar) wipes every conversation.
- Closing the window shuts down cleanly: sessions are closed and the node is stopped.
- Narrow screens (phones) show either the chat list or one conversation; back returns to the list.

Other modes, picked by environment variables:

    HOPR_CHAT_DEV_NICK=a npm run tauri dev               # localcluster node, like `hopr-chat dev`
    HOPR_CHAT_DEV_NICK=b npm run tauri:second            # 2nd window: reuses the dev server on :1420
    HOPR_CHAT_ATTACH_API=http://localhost:3002 HOPR_CHAT_NICK=me \
      HOPR_CHAT_HOPRD_CONFIG=/tmp/hopr-nodes/hoprd_cfg_2.yaml npm run tauri dev   # running hoprd

Also: `HOPR_CHAT_HOPS`, `HOPR_CHAT_HOME`, `HOPR_CHAT_DEV_NODE`,
`HOPR_CHAT_CLUSTER_DIR`. Dev modes keep data in `~/.hopr-chat/<nick>/`.

## Android

Same Tauri app; hoprd runs inside the app process and a foreground service keeps it alive in the
background. Swiping the app away from recents ends that service, after which Android may stop the
process (and the node) at any time.

Prerequisites: Android SDK (`ANDROID_HOME`), NDK r28+ (`NDK_HOME`, 16 KB pages), JDK 17,
`rustup target add aarch64-linux-android`. With Homebrew, for example:

    export ANDROID_HOME=/opt/homebrew/share/android-commandlinetools
    export NDK_HOME=$ANDROID_HOME/ndk/<version> JAVA_HOME=/opt/homebrew/opt/openjdk@17

**Signing = keeping your identity.** `adb install -r` upgrades in place and keeps the app data only
when the APK is signed with the same key; otherwise the only way out is uninstalling, which deletes the
identity. Create a release key once and back it up together with its passwords:

    keytool -genkeypair -v -keystore ~/.android/hopr-chat-release.jks -alias hopr-chat \
      -keyalg RSA -keysize 4096 -validity 10000

and point Gradle at it in `app/src-tauri/gen/android/key.properties` (gitignored):

    storeFile=/Users/<you>/.android/hopr-chat-release.jks
    storePassword=...
    keyAlias=hopr-chat
    keyPassword=...

Without `key.properties` the release APK is unsigned. Build and install (phone with USB debugging on):

    cd app
    npx tauri android build --apk -t aarch64             # ~10 min
    adb install -r src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release.apk
    adb logcat | grep RustStdoutStderr                   # core + hoprd logs

## Where things live

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
| `hopr.safe` | Safe + module address (YAML). Delete it to choose the Safe again | re-enter them ("Use an existing Safe") |
| `settings.toml` | nick, network (`blokli_url`), hops, API/p2p ports | defaults come back |
| `chat.db` | contacts, messages, delivery state (SQLite) | chat history |
| `hoprd.yaml` | hoprd config, rewritten on every start | nothing |
| `hoprd-db/` | hoprd's own database | resynced from chain |
| `hoprd.log` | hoprd + core log (desktop; Android logs to logcat) | nothing |
| `hoprd.token` | REST API token of the running node (0600), for `curl` | nothing (new each start) |

**Back up `hopr.id` + `hopr.pass` together** (or the whole home). On Android they survive updates only
if the new APK is signed with the same key; uninstall or "Clear storage" deletes them. If the app
created the Safe itself, the node key is the Safe owner, so losing it also loses the Safe's funds. A
Safe owned by a separate (Ledger) address avoids that.

## Deleting messages

The text is really gone from `chat.db`: SQLite runs with `secure_delete` (freed pages are overwritten)
and the purge checkpoints and truncates the WAL, which would otherwise keep copies. Queued messages that
were not sent yet are dropped too. It only affects this device: your peer keeps their copy, and backups
made before (Time Machine, Android backup) still have it.

## One Safe for several nodes (phone + desktop)

The in-app "Create a new Safe" makes one Safe per node, owned by that node's key. To share one Safe
between your nodes, create it with hopli (`hoprnet/hopli`):

    PRIVATE_KEY=<gas-only deployer key> hopli safe-module create \
      --network piz-palu-prod --provider-url https://rpc.gnosischain.com \
      --node-address <node1>,<node2> \
      --admin-address <owner> --threshold 1 --allowance 100

- `--admin-address` is the Safe owner: the only key that can withdraw funds or add/remove nodes. Use a
  dedicated (ideally Ledger) account. Omit it and the deployer key becomes the owner.
- Node addresses: the app shows them on the Safe screen, or
  `hopli identity read --identity-from-path <home>/hopr.id --password-path <home>/hopr.pass`.
- Fund on Gnosis Chain: wxHOPR (`0xD4fdec44DB9D44B8f2b6d529620f9C0C7066A2c1`) to the Safe, ~0.05 xDai
  gas to each node.
- In each app choose "Use an existing Safe" and enter the Safe + module addresses.

## Verify traffic really goes over HOPR

**1. On chain (no node access needed).** Your Safe's payment channels to relays, from blokli:

    curl -s https://blokli-piz-palu.prod.hoprnet.link/graphql -H 'content-type: application/json' -d \
      '{"query":"{ channels(safeAddress: \"<SAFE>\") { ... on ChannelsList { channels { source destination balance status ticketIndex } } } }"}'

`source`/`destination` are key ids; resolve them with `accounts(keyid: N) { ... on AccountsList { accounts { chainKey multiAddresses } } }`.
A relay's `ticketIndex` on your channel goes up when it redeems tickets it earned forwarding your
packets. Your own nodes show `multiAddresses: []`: nobody (including your other node) can dial them
directly.

**2. Your node's REST API**, with the token from `hoprd.token`:

    T=$(cat ~/Library/Application\ Support/hopr-chat/hoprd.token); api() { curl -s -H "X-Auth-Token: $T" localhost:3100/api/v4/$1; }
    api channels             # your outgoing channels (to relays)
    api network/connected    # peers you're connected to: relays, not your chat partner
    api session/tcp          # open sessions

**3. On the wire.** With hops = 1, packets to your partner go to the relay's IP, never to theirs:

    lsof -nP -a -p $(pgrep -f hopr-chat-app) -i UDP          # find the node's UDP port
    sudo tcpdump -ni en0 udp port <that port>                  # remote IPs = relays

Strongest test: put the phone on mobile data (no Wi-Fi). Messages still arrive both ways although
neither device can reach the other directly.

## Dev (localcluster)

A local 3-node cluster on anvil + blokli; no real funds, no internet needed. One-time setup:

1. A running container runtime: Docker (default), OrbStack (`orbctl start`), Podman, or Apple's
   `container`. Pick a non-docker one with `HOPRD_CONTAINER_RUNTIME=podman` (or `container`).
2. The chain image (amd64-only, runs under Rosetta on Apple silicon; the digest is the hoprd CI pin):

       docker pull --platform linux/amd64 \
         europe-west3-docker.pkg.dev/hoprassociation/docker-images/bloklid-anvil@sha256:75956aede10a71fbb8a46fae05d1aa1caed10ae6ed410c4385df3d565b4fe4fc

3. hoprd and hoprd-localcluster, built with nix in a checkout of the hoprd repo, ideally at the rev
   pinned in `Cargo.toml`:

       nix build .#binary-hoprd -o result-hoprd
       nix build .#binary-hoprd-localcluster -o result-localcluster
       export HOPR_CHAT_HOPRD_BIN=$PWD/result-hoprd/bin/hoprd
       export HOPR_CHAT_LOCALCLUSTER_BIN=$PWD/result-localcluster/bin/hoprd-localcluster

Then, with those two variables exported in each shell:

    cargo run -- dev --nick a     # starts the cluster if needed (first time takes minutes), attaches to node 0
    cargo run -- dev --nick b     # node 1; node 2 relays (default 1 hop)
    cargo run -- dev-down         # stops the cluster

The GUI uses the same variables: `HOPR_CHAT_DEV_NICK=a npm run tauri dev` (see GUI above).

Each nick sticks to the node it got first (remembered in `~/.hopr-chat/<nick>/dev-node`; the cluster
uses frozen identities), so its address stays the same across runs; `--node <id>` overrides it. Dev
mode signs with the node's key from `/tmp/hopr-nodes/hoprd_cfg_<id>.yaml` and keeps chat data in
`~/.hopr-chat/<nick>/`; the cluster log is `localcluster.log` there. Free ports: 8080,
3000-3002, 9000-9002. If the cluster gets stuck: `dev-down`, `docker rm -f hopr-chain`,
`rm -rf /tmp/hopr-nodes`.

`attach --api URL --nick n [--hoprd-config cfg.yaml]` uses any running hoprd. Without the config its
messages are unsigned, and contacts drop unsigned messages, so it can only talk to strangers.

`cargo build --no-default-features` spawns an external hoprd instead of embedding it, found in this
order: `hoprd_bin` in settings, `HOPR_CHAT_HOPRD_BIN`, the binary next to hopr-chat, `PATH`.

## Limits

- `--hops 0` is direct and not anonymous.
- A signed message reveals your node address to the receiver.
- There is no mailbox: both sides must be online at some point.
- An existing Safe must already include this node in its module. Otherwise hoprd fails at startup;
  check `hoprd.log`.
- Not done yet: signed update manifest, store builds, a faucet for test funds.

## License

GPL-3.0-only, see [LICENSE](LICENSE) (same as hoprd and hopr-lib, which are linked in).
