export const toasts = $state([]);
let next = 0;

/** `kind`: "info" | "error" | "success". `action` is an optional `{ label, run }`. */
export function toast(text, kind = "info", action = null, ttl = 6000) {
  const id = next++;
  toasts.push({ id, text, kind, action });
  setTimeout(() => dismiss(id), ttl);
}

export function dismiss(id) {
  const i = toasts.findIndex((t) => t.id === id);
  if (i >= 0) toasts.splice(i, 1);
}
