<script>
  import { api, on } from "./lib/api.js";
  import Onboarding from "./lib/Onboarding.svelte";
  import Chat from "./lib/Chat.svelte";
  import Toasts from "./lib/Toasts.svelte";

  let snapshot = $state({ statuses: [], choose_safe: null, info: null, fatal: null });

  $effect(() => {
    const stops = [
      on("status", (s) => snapshot.statuses.push(s)),
      on("choose_safe", (address) => (snapshot.choose_safe = address)),
      on("ready", (info) => (snapshot.info = info)),
      on("fatal", (message) => (snapshot.fatal = message)),
    ];
    // Events fired before these listeners existed are only in the backend snapshot.
    api.snapshot().then((s) => (snapshot = s));
    return () => stops.forEach((stop) => stop());
  });
</script>

{#if snapshot.info && !snapshot.fatal}
  <Chat info={snapshot.info} />
{:else}
  <Onboarding bind:snapshot />
{/if}
<Toasts />
