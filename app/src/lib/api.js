import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";

export const api = {
  snapshot: () => invoke("snapshot"),
  answerSafe: (choice) => invoke("answer_safe", { choice }),
  conversations: () => invoke("conversations"),
  peerNames: () => invoke("peer_names"),
  history: (peer, limit = 500) => invoke("history", { peer, limit }),
  send: (peer, text) => invoke("send", { peer, text }),
  addPeer: (nick, address) => invoke("add_peer", { nick, address }),
  peers: () => invoke("peers"),
  wallet: () => invoke("wallet"),
};

/** Subscribes to a backend event; returns an unsubscribe function usable in `$effect` cleanup. */
export function on(event, handler) {
  const unlisten = listen(event, (e) => handler(e.payload));
  return () => unlisten.then((stop) => stop());
}
