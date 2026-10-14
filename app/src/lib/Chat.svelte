<script>
  import { onMount, tick } from "svelte";
  import { api, on } from "./api.js";
  import { ago, clock, day, short, initials, hue, isAddress, copy } from "./format.js";
  import { toast } from "./toasts.svelte.js";
  import Modal from "./Modal.svelte";

  let { info } = $props();

  let convs = $state([]);
  let names = $state([]);
  let selected = $state(null);
  let msgs = $state([]);
  let wallet = $state(null);
  let search = $state("");
  let draft = $state("");
  let sending = $state(false);
  let modal = $state(null); // "add" | "wallet" | "share" | "purge" | "remove" | null
  let purgePeer = $state(null); // conversation to purge; null = every conversation
  let newNick = $state("");
  let newAddress = $state("");
  let adding = $state(false);
  let scroller = $state();
  let composer = $state();
  /** Phones show either the chat list or one conversation, never both. */
  const phone = window.matchMedia("(max-width: 700px)");
  let narrow = $state(phone.matches);
  /** Last unreachable toast per peer, so retries every 15s don't flood the screen. */
  const lastUnreachable = new Map();

  /** Conversations first, then saved peers that have no messages yet. */
  const chats = $derived.by(() => {
    const list = convs.map((c) => ({ ...c }));
    for (const n of names) if (!list.some((c) => c.peer === n)) list.push({ peer: n, last_text: "", last_at: 0, unread: 0 });
    const q = search.trim().toLowerCase();
    return q ? list.filter((c) => c.peer.toLowerCase().includes(q)) : list;
  });

  const critical = $derived(wallet?.warnings.some(([c]) => c));

  /** Groups consecutive messages by day for date separators. */
  const timeline = $derived.by(() => {
    const out = [];
    let lastDay = null;
    for (const m of msgs) {
      const d = day(m.created);
      if (d !== lastDay) out.push({ separator: d, key: `d-${d}` });
      lastDay = d;
      out.push({ msg: m, key: `${m.dir}-${m.id}` });
    }
    return out;
  });

  async function loadChats() {
    try {
      [convs, names] = await Promise.all([api.conversations(), api.peerNames()]);
      if (!selected && !narrow && chats.length) await open(chats[0].peer);
    } catch (e) {
      toast(String(e), "error");
    }
  }

  async function loadMessages(stick = true) {
    if (!selected) return;
    const nearBottom = !scroller || scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 120;
    msgs = await api.history(selected);
    if (stick || nearBottom) {
      await tick();
      scroller?.scrollTo({ top: scroller.scrollHeight });
    }
  }

  async function open(peer) {
    // A history entry lets Android's back button return to the list.
    if (narrow && !selected) history.pushState({ chat: peer }, "");
    selected = peer;
    draft = "";
    await loadMessages(true);
    convs = await api.conversations();
    composer?.focus();
  }

  async function refreshWallet() {
    try {
      const before = critical;
      wallet = await api.wallet();
      const bad = wallet.warnings.find(([c]) => c);
      if (bad && !before) toast(bad[1], "error", { label: "Details", run: () => (modal = "wallet") }, 10000);
    } catch {
      wallet = null;
    }
  }

  async function send() {
    const text = draft.trim();
    if (!text || !selected || sending) return;
    sending = true;
    try {
      await api.send(selected, text);
      draft = "";
      await loadMessages(true);
      convs = await api.conversations();
    } catch (e) {
      toast(String(e), "error");
    } finally {
      sending = false;
      composer?.focus();
    }
  }

  async function addPeer(e) {
    e.preventDefault();
    adding = true;
    try {
      await api.addPeer(newNick, newAddress);
      modal = null;
      const nick = newNick.trim();
      newNick = newAddress = "";
      await loadChats();
      await open(nick);
      toast(`Added ${nick}`, "success", null, 2500);
    } catch (err) {
      toast(String(err), "error");
    } finally {
      adding = false;
    }
  }

  function askPurge(peer) {
    purgePeer = peer;
    modal = "purge";
  }

  async function purge() {
    try {
      const deleted = await api.purge(purgePeer);
      modal = null;
      if (purgePeer === null || purgePeer === selected) msgs = [];
      convs = await api.conversations();
      toast(`Deleted ${deleted} message${deleted === 1 ? "" : "s"}`, "success", null, 2500);
    } catch (e) {
      toast(String(e), "error");
    }
  }

  async function removeContact() {
    const nick = selected;
    try {
      const deleted = await api.removePeer(nick);
      modal = null;
      selected = null;
      msgs = [];
      if (history.state?.chat) history.back();
      await loadChats();
      toast(`Removed ${nick} and ${deleted} message${deleted === 1 ? "" : "s"}`, "success", null, 2500);
    } catch (e) {
      toast(String(e), "error");
    }
  }

  async function onEvent(ev) {
    const { kind, data } = ev;
    if (kind === "message") {
      if (data.peer === selected) await loadMessages(false);
      else {
        toast(`${short(data.peer)}: ${data.text}`, "info", { label: "Open", run: () => open(data.peer) });
      }
      convs = await api.conversations();
      names = await api.peerNames();
    } else if (kind === "delivered") {
      if (data.peer === selected) await loadMessages(false);
    } else if (kind === "unreachable") {
      const last = lastUnreachable.get(data.peer) ?? 0;
      if (Date.now() - last > 60000) {
        lastUnreachable.set(data.peer, Date.now());
        toast(`${data.peer} is unreachable, messages stay queued and are retried`, "error");
      }
      if (data.peer === selected) await loadMessages(false);
    } else if (kind === "warning") {
      toast(data, "error");
    }
  }

  function composerKey(e) {
    if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
      e.preventDefault();
      send();
    }
  }

  function globalKey(e) {
    if ((e.metaKey || e.ctrlKey) && e.key === "k") {
      e.preventDefault();
      document.getElementById("search")?.focus();
    } else if ((e.metaKey || e.ctrlKey) && e.key === "n") {
      e.preventDefault();
      modal = "add";
    }
  }

  async function copyText(text, label) {
    await copy(text);
    toast(`${label} copied`, "success", null, 2000);
  }

  // onMount, not $effect: loading reads reactive state and must not re-run on every change.
  onMount(() => {
    loadChats();
    refreshWallet();
    const timer = setInterval(refreshWallet, 30000);
    // Keeps relative times ("5m") fresh.
    const clockTimer = setInterval(() => (convs = [...convs]), 60000);
    const stop = on("chat_event", onEvent);
    phone.onchange = (e) => (narrow = e.matches);
    const back = () => (selected = null);
    window.addEventListener("popstate", back);
    return () => {
      window.removeEventListener("popstate", back);
      clearInterval(timer);
      clearInterval(clockTimer);
      stop();
    };
  });

  // Auto-grow the composer up to a few lines.
  $effect(() => {
    draft;
    if (!composer) return;
    composer.style.height = "auto";
    composer.style.height = `${Math.min(composer.scrollHeight, 140)}px`;
  });
</script>

<svelte:window onkeydown={globalKey} />

<div class="layout {narrow ? (selected ? 'narrow show-chat' : 'narrow') : ''}">
  <aside>
    <div class="me">
      <div class="avatar" style:--h={hue(info.nick)}>{initials(info.nick)}</div>
      <div class="who">
        <strong>{info.nick}</strong>
        <button class="ghost addr mono" title="Copy your address" onclick={() => copyText(info.me, "Address")}>{short(info.me)}</button>
      </div>
      <button class="icon" title="Share your address" onclick={() => (modal = "share")}>⇪</button>
    </div>
    <div class="tags">
      <span class="badge {info.signing ? 'ok' : 'warn'}" title={info.signing ? "Messages are signed with your node key" : "No node key: messages are unsigned"}>
        {info.signing ? "✓ signed" : "unsigned"}
      </span>
      <span class="badge {info.hops === 0 ? 'bad' : ''}" title="Relay hops each way">
        {info.hops === 0 ? "0 hops · not anonymous" : `${info.hops} hop${info.hops === 1 ? "" : "s"}`}
      </span>
    </div>

    <div class="search">
      <input id="search" bind:value={search} placeholder={narrow ? "Search chats" : "Search chats  ⌘K"} spellcheck="false" />
      <button class="primary new" title="New chat (⌘N)" onclick={() => (modal = "add")}>＋</button>
    </div>

    <nav>
      {#each chats as c (c.peer)}
        <button class="chat {c.peer === selected ? 'active' : ''}" onclick={() => open(c.peer)}>
          <div class="avatar" style:--h={hue(c.peer)}>{initials(c.peer)}</div>
          <div class="text">
            <div class="line">
              <strong class={c.unread ? "unread" : ""}>{short(c.peer)}</strong>
              {#if c.last_at}<span class="muted small">{ago(c.last_at)}</span>{/if}
            </div>
            <div class="line">
              <span class="muted preview">{c.last_text || "No messages yet"}</span>
              {#if c.unread}<span class="count">{c.unread}</span>{/if}
            </div>
          </div>
        </button>
      {:else}
        <div class="empty-list muted">
          {search ? "No chat matches" : "No chats yet."}
          {#if !search}<button class="ghost link" onclick={() => (modal = "add")}>Add a peer</button>{/if}
        </div>
      {/each}
    </nav>

    <button class="wallet {critical ? 'bad' : wallet?.warnings.length ? 'warn' : ''}" onclick={() => (modal = "wallet")} title="Balances">
      {#if wallet}
        <span><span class="muted">gas</span> {wallet.gas}</span>
        <span><span class="muted">funds</span> {wallet.funds}</span>
        {#if wallet.warnings.length}<span>⚠</span>{/if}
      {:else}
        <span class="muted">balance unavailable</span>
      {/if}
    </button>
    <button class="ghost purge-all" onclick={() => askPurge(null)}>Delete all messages</button>
  </aside>

  <section class="conversation">
    {#if selected}
      <header>
        {#if narrow}<button class="ghost back" title="Back" onclick={() => (history.state?.chat ? history.back() : (selected = null))}>‹</button>{/if}
        <div class="avatar" style:--h={hue(selected)}>{initials(selected)}</div>
        <div class="who">
          <strong>{short(selected)}</strong>
          {#if isAddress(selected)}
            <span class="muted small">Not in your contacts. Replies go to this node address.</span>
          {/if}
        </div>
        <button class="ghost clear" title="Delete this conversation's messages" onclick={() => askPurge(selected)}>Clear</button>
        {#if !isAddress(selected)}
          <button class="ghost clear" title="Delete this contact and its messages" onclick={() => (modal = "remove")}>Delete contact</button>
        {/if}
      </header>

      <div class="messages" bind:this={scroller}>
        {#each timeline as item (item.key)}
          {#if item.separator}
            <div class="separator"><span>{item.separator}</span></div>
          {:else}
            {@const m = item.msg}
            <div class="bubble-row {m.dir}">
              <div class="bubble">
                {#if m.dir === "in"}
                  <div class="meta-top">
                    <!-- The saved contact name (or the stranger's address), never the nick the sender claims. -->
                    <span class="nick" style:--h={hue(m.peer)}>{short(m.peer)}</span>
                    {#if m.nick && isAddress(m.peer)}<span class="muted small">~{m.nick}</span>{/if}
                    {#if m.verified === "verified"}
                      <span class="sig ok" title="Signed by the sender's node key">✓ verified</span>
                    {:else if m.verified === "invalid"}
                      <span class="sig bad" title="Signature does not match the claimed sender">✗ forged?</span>
                    {:else}
                      <span class="sig" title="Sender did not sign this message (only strangers may skip signing)">unsigned</span>
                    {/if}
                  </div>
                {/if}
                <div class="body">{m.text}</div>
                <div class="meta">
                  {clock(m.created)}
                  {#if m.dir === "out"}
                    <span class="status {m.status}" title={m.status}>
                      {m.status === "delivered" ? "✓✓" : m.status === "sent" ? "✓" : "🕓"}
                    </span>
                  {/if}
                </div>
              </div>
            </div>
          {/if}
        {:else}
          <div class="empty muted">No messages yet. Say hi 👋</div>
        {/each}
      </div>

      <footer>
        <textarea
          bind:this={composer}
          bind:value={draft}
          onkeydown={composerKey}
          maxlength="4000"
          rows="1"
          placeholder={narrow ? "Message" : `Message ${short(selected)}  (Enter to send, Shift+Enter for a new line)`}
        ></textarea>
        <button class="primary" disabled={!draft.trim() || sending} onclick={send}>Send</button>
      </footer>
    {:else}
      <div class="welcome">
        <h2>Welcome, {info.nick}</h2>
        <p class="muted">Messages travel over the HOPR mixnet, signed with your node key.</p>
        <div class="row">
          <button class="primary" onclick={() => (modal = "add")}>Add a peer</button>
          <button onclick={() => (modal = "share")}>Share my address</button>
        </div>
      </div>
    {/if}
  </section>
</div>

{#if modal === "add"}
  <Modal title="New chat" onclose={() => (modal = null)}>
    <form class="form" onsubmit={addPeer}>
      <label>Nickname <input bind:value={newNick} placeholder="bob" spellcheck="false" /></label>
      <label>Node address <input bind:value={newAddress} placeholder="0x…" spellcheck="false" class="mono" /></label>
      <p class="muted small">Ask your peer for their address (Share my address). The node must be announced on the network.</p>
      <div class="row end">
        <button type="button" class="ghost" onclick={() => (modal = null)}>Cancel</button>
        <button class="primary" disabled={adding || !newNick.trim() || !isAddress(newAddress)}>{adding ? "Checking…" : "Add"}</button>
      </div>
    </form>
  </Modal>
{:else if modal === "purge"}
  <Modal title={purgePeer === null ? "Delete all messages" : `Clear chat with ${short(purgePeer)}`} onclose={() => (modal = null)}>
    <p>
      {purgePeer === null ? "Every message in every chat" : "Every message in this chat"} is wiped from this device's database,
      including queued messages that were not sent yet. Contacts stay. Your peers keep their own copies.
    </p>
    <div class="row end">
      <button class="ghost" onclick={() => (modal = null)}>Cancel</button>
      <button class="danger" onclick={purge}>Delete</button>
    </div>
  </Modal>
{:else if modal === "remove"}
  <Modal title={`Delete contact ${selected}`} onclose={() => (modal = null)}>
    <p>
      The contact and every message in this chat are wiped from this device, including queued messages that were
      not sent yet. If they write to you again, they show up by address.
    </p>
    <div class="row end">
      <button class="ghost" onclick={() => (modal = null)}>Cancel</button>
      <button class="danger" onclick={removeContact}>Delete</button>
    </div>
  </Modal>
{:else if modal === "share"}
  <Modal title="Share your address" onclose={() => (modal = null)}>
    <p class="muted">Anyone with this address can start a chat with you.</p>
    <button class="copybox mono" onclick={() => copyText(info.me, "Address")}>{info.me}<span class="muted">copy</span></button>
    <button class="copybox mono" onclick={() => copyText(`/add ${info.nick} ${info.me}`, "Command")}>/add {info.nick} {short(info.me)}<span class="muted">copy for terminal users</span></button>
  </Modal>
{:else if modal === "wallet"}
  <Modal title="Balances" onclose={() => (modal = null)} width={460}>
    {#if wallet}
      <div class="wallet-section">
        <strong>Node wallet</strong> <span class="muted small">pays gas for channel and ticket transactions</span>
        <div class="kv"><span class="muted">gas</span><span>{wallet.gas}</span></div>
        <div class="kv"><span class="muted">wxHOPR</span><span>{wallet.node_wxhopr} <span class="muted small">(normally 0, it lives in the Safe)</span></span></div>
      </div>
      <div class="wallet-section">
        <strong>Safe</strong> <span class="muted small">holds the wxHOPR that funds channels to relays</span>
        <div class="kv"><span class="muted">funds</span><span>{wallet.funds}</span></div>
        <div class="kv"><span class="muted">allowance</span><span>{wallet.allowance}</span></div>
        <div class="kv"><span class="muted">xDai</span><span>{wallet.safe_xdai}</span></div>
      </div>
      {#each wallet.warnings as [isCritical, text]}
        <p class={isCritical ? "bad" : "warn"}>{isCritical ? "✗" : "!"} {text}</p>
      {:else}
        <p class="ok">✓ Ready to pay relays</p>
      {/each}
    {:else}
      <p class="muted">Balance unavailable.</p>
    {/if}
  </Modal>
{/if}

<style>
  .layout {
    height: 100%;
    display: grid;
    grid-template-columns: 300px 1fr;
  }
  .layout.narrow {
    grid-template-columns: 1fr;
  }
  .layout.narrow > .conversation,
  .layout.show-chat > aside {
    display: none;
  }
  .layout.show-chat > .conversation {
    display: flex;
  }
  .back {
    font-size: 26px;
    line-height: 1;
    padding: 0 8px 4px 0;
  }
  aside {
    background: var(--panel);
    border-right: 1px solid var(--border);
    display: flex;
    flex-direction: column;
    min-height: 0;
  }
  .me {
    display: flex;
    align-items: center;
    gap: 10px;
    padding: 16px 14px 8px;
  }
  .who {
    display: flex;
    flex-direction: column;
    min-width: 0;
    flex: 1;
  }
  .addr {
    padding: 0;
    text-align: left;
    color: var(--muted);
  }
  .icon {
    padding: 4px 10px;
  }
  .tags {
    display: flex;
    gap: 6px;
    padding: 0 14px 12px;
    flex-wrap: wrap;
  }
  .search {
    display: flex;
    gap: 8px;
    padding: 0 14px 10px;
  }
  .search input {
    flex: 1;
    min-width: 0;
  }
  .new {
    padding: 6px 12px;
    font-size: 16px;
  }
  nav {
    flex: 1;
    overflow-y: auto;
    padding: 0 8px;
  }
  .chat {
    width: 100%;
    display: flex;
    gap: 10px;
    align-items: center;
    border: none;
    background: transparent;
    padding: 10px 8px;
    border-radius: 10px;
    text-align: left;
  }
  .chat:hover {
    background: var(--panel-2);
  }
  .chat.active {
    background: var(--accent-soft);
  }
  .text {
    flex: 1;
    min-width: 0;
  }
  .line {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 8px;
  }
  .unread {
    color: var(--accent);
  }
  .preview {
    white-space: nowrap;
    overflow: hidden;
    text-overflow: ellipsis;
    font-size: 13px;
  }
  .count {
    background: var(--accent);
    color: var(--accent-text);
    border-radius: 999px;
    font-size: 11px;
    font-weight: 700;
    padding: 1px 7px;
  }
  .small {
    font-size: 12px;
  }
  .avatar {
    --h: 220;
    width: 38px;
    height: 38px;
    flex: none;
    border-radius: 50%;
    display: grid;
    place-items: center;
    font-weight: 700;
    font-size: 13px;
    color: hsl(var(--h) 60% 30%);
    background: hsl(var(--h) 70% 85%);
  }
  .empty-list {
    padding: 20px 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
    align-items: flex-start;
  }
  .link {
    padding: 0;
    color: var(--accent);
  }
  .wallet {
    margin: 10px 14px 14px;
    display: flex;
    flex-wrap: wrap;
    justify-content: space-between;
    gap: 8px;
    background: var(--panel-2);
    font-size: 13px;
  }
  .wallet.warn {
    border-color: var(--warn);
  }
  .wallet.bad {
    border-color: var(--bad);
    color: var(--bad);
  }
  .conversation {
    display: flex;
    flex-direction: column;
    min-height: 0;
    min-width: 0;
  }
  .conversation header {
    display: flex;
    align-items: center;
    gap: 12px;
    padding: 12px 20px;
    border-bottom: 1px solid var(--border);
    background: var(--panel);
  }
  .messages {
    flex: 1;
    overflow-y: auto;
    padding: 18px 20px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .separator {
    text-align: center;
    margin: 10px 0;
  }
  .separator span {
    font-size: 11.5px;
    color: var(--muted);
    background: var(--panel-2);
    padding: 3px 10px;
    border-radius: 999px;
  }
  .bubble-row {
    display: flex;
  }
  .bubble-row.out {
    justify-content: flex-end;
  }
  .bubble {
    max-width: min(70%, 560px);
    padding: 8px 12px 6px;
    border-radius: 16px;
    background: var(--bubble-in);
    border: 1px solid var(--border);
  }
  .out .bubble {
    background: var(--bubble-out);
    color: var(--bubble-out-text);
    border-color: transparent;
    border-bottom-right-radius: 4px;
  }
  .in .bubble {
    border-bottom-left-radius: 4px;
  }
  .meta-top {
    display: flex;
    gap: 8px;
    align-items: baseline;
    margin-bottom: 2px;
  }
  .nick {
    --h: 220;
    font-weight: 700;
    font-size: 12.5px;
    color: hsl(var(--h) 55% 45%);
  }
  .sig {
    font-size: 11px;
    color: var(--muted);
  }
  .sig.ok {
    color: var(--ok);
  }
  .sig.bad {
    color: var(--bad);
    font-weight: 700;
  }
  .body {
    white-space: pre-wrap;
    word-break: break-word;
    line-height: 1.45;
  }
  .meta {
    font-size: 11px;
    opacity: 0.7;
    text-align: right;
    margin-top: 2px;
  }
  .status.delivered {
    font-weight: 700;
  }
  .empty {
    margin: auto;
  }
  footer {
    display: flex;
    gap: 10px;
    align-items: flex-end;
    padding: 12px 20px 16px;
    border-top: 1px solid var(--border);
    background: var(--panel);
  }
  footer textarea {
    flex: 1;
    resize: none;
    line-height: 1.4;
    max-height: 140px;
  }
  .welcome {
    margin: auto;
    text-align: center;
    display: flex;
    flex-direction: column;
    gap: 10px;
    align-items: center;
  }
  .welcome h2 {
    margin: 0;
  }
  .row {
    display: flex;
    gap: 10px;
  }
  .row.end {
    justify-content: flex-end;
  }
  .form {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  .form label {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-weight: 600;
    font-size: 13px;
  }
  .form p,
  .wallet-section,
  p {
    margin: 0;
  }
  .copybox {
    display: flex;
    justify-content: space-between;
    align-items: center;
    gap: 10px;
    background: var(--panel-2);
    text-align: left;
    word-break: break-all;
  }
  .wallet-section {
    display: flex;
    flex-direction: column;
    gap: 6px;
    background: var(--panel-2);
    border-radius: var(--radius);
    padding: 12px 14px;
  }
  .purge-all {
    align-self: center;
    margin: -6px 14px 10px;
    font-size: 12.5px;
    color: var(--muted);
  }
  .who strong {
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .clear {
    margin-left: 4px;
    padding: 6px 8px;
    white-space: nowrap;
    font-size: 13px;
    color: var(--muted);
  }
  .danger {
    background: var(--bad);
    border-color: var(--bad);
    color: #fff;
  }
  .kv {
    display: flex;
    justify-content: space-between;
    gap: 12px;
  }
  .kv > :last-child {
    min-width: 0;
    text-align: right;
    overflow-wrap: anywhere;
  }
  p.ok {
    color: var(--ok);
  }
  p.warn {
    color: var(--warn);
  }
  p.bad {
    color: var(--bad);
  }
</style>
