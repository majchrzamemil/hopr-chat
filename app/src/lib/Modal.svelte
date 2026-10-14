<script>
  let { title, onclose, children, width = 440 } = $props();
</script>

<svelte:window onkeydown={(e) => e.key === "Escape" && onclose()} />

<!-- svelte-ignore a11y_click_events_have_key_events, a11y_no_static_element_interactions -->
<div class="backdrop" onclick={onclose}>
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <div class="modal" style:width="min({width}px, 100%)" role="dialog" tabindex="-1" aria-modal="true" aria-label={title} onclick={(e) => e.stopPropagation()}>
    <header>
      <h2>{title}</h2>
      <button class="ghost close" onclick={onclose} aria-label="Close">✕</button>
    </header>
    {@render children()}
  </div>
</div>

<style>
  .backdrop {
    position: fixed;
    inset: 0;
    background: rgba(10, 12, 20, 0.45);
    display: grid;
    /* minmax(0, …) or the column grows to the modal's width and pushes it off narrow screens */
    grid-template-columns: minmax(0, 1fr);
    place-items: center;
    z-index: 20;
    padding: 20px 12px;
  }
  .modal {
    max-width: 100%;
    max-height: 100%;
    overflow-y: auto;
    background: var(--panel);
    border: 1px solid var(--border);
    border-radius: 16px;
    box-shadow: var(--shadow);
    padding: 20px 22px 22px;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }
  header {
    display: flex;
    align-items: center;
    justify-content: space-between;
  }
  h2 {
    margin: 0;
    font-size: 17px;
  }
  .close {
    padding: 4px 8px;
    color: var(--muted);
  }
</style>
