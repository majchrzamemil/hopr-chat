export function ago(ts) {
  const s = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (s < 60) return "now";
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  if (s < 86400) return `${Math.floor(s / 3600)}h`;
  return `${Math.floor(s / 86400)}d`;
}

export function clock(ts) {
  return new Date(ts * 1000).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

export function day(ts) {
  return new Date(ts * 1000).toLocaleDateString([], { weekday: "short", day: "numeric", month: "short" });
}

/** Senders that were never added show as their address; keep it readable. */
export function short(label) {
  return label?.startsWith("0x") && label.length > 14 ? `${label.slice(0, 6)}…${label.slice(-4)}` : label;
}

export function initials(label) {
  return label?.startsWith("0x") ? label.slice(2, 4).toUpperCase() : (label ?? "?").slice(0, 2).toUpperCase();
}

/** Stable per-peer avatar hue. */
export function hue(label) {
  let h = 0;
  for (const c of label ?? "") h = (h * 31 + c.charCodeAt(0)) % 360;
  return h;
}

export const isAddress = (s) => /^0x[0-9a-fA-F]{40}$/.test(s.trim());

export async function copy(text) {
  await navigator.clipboard.writeText(text);
}
