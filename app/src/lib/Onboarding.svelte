<script>
  import { api } from "./api.js";
  import { copy, isAddress } from "./format.js";
  import { toast } from "./toasts.svelte.js";

  let { snapshot = $bindable() } = $props();

  let mode = $state(null); // null | "existing"
  let safe = $state("");
  let module = $state("");
  let submitting = $state(false);

  const latest = (kind) => snapshot.statuses.findLast((s) => s.kind === kind);
  const identity = $derived(latest("identity"));
  const funds = $derived(latest("waiting_for_funds"));
  const unsigned = $derived(latest("unsigned"));
  const current = $derived(snapshot.statuses.at(-1));

  /** Human headline for the step the backend is in right now. */
  const headline = $derived.by(() => {
    if (snapshot.fatal) return "Could not start";
    if (snapshot.choose_safe) return "Set up your Safe";
    switch (current?.kind) {
      case "waiting_for_funds":
        return "Fund your node";
      case "deploying_safe":
        return "Deploying your Safe…";
      case "starting_hoprd":
        return "Starting your HOPR node…";
      case "starting_cluster":
        return "Attaching to localcluster…";
      case "ready":
        return "Opening chat…";
      default:
        return "Starting…";
    }
  });

  async function choose(choice) {
    submitting = true;
    try {
      await api.answerSafe(choice);
      snapshot.choose_safe = null;
    } catch (e) {
      toast(String(e), "error");
    } finally {
      submitting = false;
    }
  }

  async function copyAddress(address) {
    await copy(address);
    toast("Address copied", "success", null, 2000);
  }
</script>

<main>
  <section class="card">
    <header>
      <div class="logo">h</div>
      <div>
        <h1>{headline}</h1>
        {#if identity}
          <p class="muted">Node <span class="mono">{identity.address}</span></p>
        {/if}
      </div>
      {#if !snapshot.fatal && !snapshot.choose_safe && current?.kind !== "waiting_for_funds"}
        <div class="spinner"></div>
      {/if}
    </header>

    {#if snapshot.fatal}
      <pre class="fatal">{snapshot.fatal}</pre>
      <p class="muted">Check <span class="mono">hoprd.log</span> in the app data folder (logcat on Android), fix the cause and restart the app.</p>
    {:else if snapshot.choose_safe}
      <p>This node has no Safe yet. The Safe holds the wxHOPR that pays relays; the node only needs a little xDai for gas.</p>
      {#if mode === "existing"}
        <form
          onsubmit={(e) => {
            e.preventDefault();
            choose({ kind: "existing", safe: safe.trim(), module: module.trim() });
          }}
        >
          <label>Safe address <input bind:value={safe} placeholder="0x…" spellcheck="false" /></label>
          <label>Module address <input bind:value={module} placeholder="0x…" spellcheck="false" /></label>
          <p class="muted small">This node ({snapshot.choose_safe}) must already be added to the Safe's module, e.g. in the HOPR staking hub.</p>
          <div class="row">
            <button type="button" class="ghost" onclick={() => (mode = null)}>Back</button>
            <button class="primary" disabled={submitting || !isAddress(safe) || !isAddress(module)}>Use this Safe</button>
          </div>
        </form>
      {:else}
        <div class="choices">
          <button class="choice" onclick={() => (mode = "existing")}>
            <strong>Use an existing Safe</strong>
            <span class="muted">You already manage a Safe with this node in its module.</span>
          </button>
          <button class="choice" disabled={submitting} onclick={() => choose({ kind: "create" })}>
            <strong>Create a new Safe</strong>
            <span class="muted">Send xDai + wxHOPR to this node; the app deploys the Safe for you.</span>
          </button>
        </div>
      {/if}
    {:else if current?.kind === "waiting_for_funds" && funds}
      <p>Send funds on <strong>Gnosis Chain</strong> to your node address. The app deploys your Safe as soon as they arrive.</p>
      <button class="address" onclick={() => copyAddress(funds.address)} title="Copy">
        <span class="mono">{funds.address}</span><span class="muted small">copy</span>
      </button>
      <div class="needs">
        <div><span class="muted">xDai (gas)</span><strong>{funds.xdai}</strong><span class="muted">of {funds.need_xdai}</span></div>
        <div><span class="muted">wxHOPR</span><strong>{funds.wxhopr}</strong><span class="muted">of {funds.need_wxhopr}</span></div>
      </div>
      <p class="muted small row"><span class="spinner"></span> Checking balance every 10 seconds…</p>
    {:else}
      <ol class="steps">
        {#each snapshot.statuses as s}
          {#if s.kind === "identity"}
            <li>{s.created ? "Created" : "Loaded"} node identity</li>
          {:else if s.kind === "safe_ready"}
            <li>Safe <span class="mono">{s.safe}</span></li>
          {:else if s.kind === "deploying_safe"}
            <li>Deploying Safe</li>
          {:else if s.kind === "starting_hoprd"}
            <li>Starting hoprd <span class="muted small">(first start registers and announces the node on chain, this can take minutes)</span></li>
          {:else if s.kind === "starting_cluster"}
            <li>Attaching to localcluster</li>
          {/if}
        {/each}
      </ol>
    {/if}

    {#if unsigned}
      <p class="warning">Messages will be unsigned: {unsigned.reason}</p>
    {/if}
  </section>
</main>

<style>
  main {
    height: 100%;
    display: grid;
    grid-template-columns: minmax(0, 560px);
    place-content: center;
    padding: 24px;
  }
  .card {
    width: min(560px, 100%);
    min-width: 0;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 16px;
    box-shadow: var(--shadow);
    padding: 28px;
    display: flex;
    flex-direction: column;
    gap: 16px;
  }
  header {
    display: flex;
    align-items: center;
    gap: 14px;
  }
  header > div:nth-child(2) {
    flex: 1;
    min-width: 0;
  }
  h1 {
    margin: 0;
    font-size: 19px;
  }
  header p {
    margin: 4px 0 0;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .logo {
    width: 44px;
    height: 44px;
    border-radius: 12px;
    background: var(--accent);
    color: var(--accent-text);
    display: grid;
    place-items: center;
    font-weight: 800;
    font-size: 22px;
  }
  p {
    margin: 0;
    line-height: 1.5;
  }
  .small {
    font-size: 12.5px;
  }
  .row {
    display: flex;
    align-items: center;
    gap: 10px;
    justify-content: flex-end;
  }
  p.row {
    justify-content: flex-start;
  }
  .choices {
    display: grid;
    gap: 10px;
  }
  .choice {
    display: flex;
    flex-direction: column;
    align-items: flex-start;
    gap: 4px;
    text-align: left;
    padding: 14px 16px;
  }
  form {
    display: flex;
    flex-direction: column;
    gap: 12px;
  }
  label {
    display: flex;
    flex-direction: column;
    gap: 6px;
    font-weight: 600;
    font-size: 13px;
  }
  .address {
    overflow-wrap: anywhere;
    text-align: left;
    gap: 10px;
    display: flex;
    justify-content: space-between;
    align-items: center;
    background: var(--panel-2);
    padding: 12px 14px;
  }
  .needs {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 10px;
  }
  .needs div {
    background: var(--panel-2);
    border-radius: var(--radius);
    padding: 12px 14px;
    display: flex;
    flex-direction: column;
    gap: 2px;
  }
  .needs strong {
    font-size: 17px;
  }
  .steps {
    margin: 0;
    padding-left: 20px;
    display: flex;
    flex-direction: column;
    gap: 8px;
  }
  .fatal {
    white-space: pre-wrap;
    background: var(--panel-2);
    color: var(--bad);
    padding: 12px;
    border-radius: 8px;
    margin: 0;
    max-height: 240px;
    overflow: auto;
  }
  .warning {
    color: var(--warn);
    font-size: 13px;
  }
</style>
