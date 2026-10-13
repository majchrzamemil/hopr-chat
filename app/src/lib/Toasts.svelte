<script>
  import { toasts, dismiss } from "./toasts.svelte.js";
</script>

<div class="stack" aria-live="polite">
  {#each toasts as t (t.id)}
    <div class="toast {t.kind}">
      <span>{t.text}</span>
      {#if t.action}
        <button class="ghost" onclick={() => { t.action.run(); dismiss(t.id); }}>{t.action.label}</button>
      {/if}
      <button class="ghost x" onclick={() => dismiss(t.id)} aria-label="Dismiss">✕</button>
    </div>
  {/each}
</div>

<style>
  .stack {
    position: fixed;
    right: 16px;
    bottom: 16px;
    display: flex;
    flex-direction: column;
    gap: 8px;
    z-index: 30;
    max-width: 380px;
  }
  .toast {
    display: flex;
    align-items: center;
    gap: 8px;
    background: var(--panel);
    border: 1px solid var(--border);
    border-left: 4px solid var(--accent);
    border-radius: 10px;
    box-shadow: var(--shadow);
    padding: 10px 8px 10px 14px;
  }
  .toast span {
    flex: 1;
    line-height: 1.4;
    word-break: break-word;
  }
  .toast.error {
    border-left-color: var(--bad);
  }
  .toast.success {
    border-left-color: var(--ok);
  }
  button {
    padding: 4px 8px;
    color: var(--accent);
    font-weight: 600;
  }
  .x {
    color: var(--muted);
  }
</style>
