import { listen, type UnlistenFn } from "@tauri-apps/api/event";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { useSyncExternalStore } from "react";

/**
 * Single source of truth for "is the host window focused". Primary feed: the
 * Rust-side GetForegroundWindow watcher (`app://focused`, see
 * src-tauri/src/focus.rs). Tauri's own focus events stay wired as a faster
 * secondary signal. Every looping animation gates on this, which is what
 * keeps the app near 0% CPU while it sits in the background.
 */
let focused = true;
const listeners = new Set<() => void>();

function set(next: boolean) {
  if (next === focused) return;
  focused = next;
  // Snapshot before notifying: a subscriber that unsubscribes during the
  // notification pass must not mutate the set we are iterating.
  for (const l of [...listeners]) l();
}

/**
 * MEMORY LEAK FIX: the previous version pushed one entry into `eventLog` on
 * every focus change and never trimmed it, while only ever reading the last
 * ten. In a tray-resident app that the user alt-tabs past dozens of times an
 * hour, that array grew for the entire uptime of the process. Bounded ring,
 * and DEV-only - it exists purely for the devtools helper below.
 */
const EVENT_LOG_MAX = 20;
const eventLog: Array<{ t: number; focused: boolean; src: string }> = [];

function record(next: boolean, src: string) {
  if (import.meta.env.DEV) {
    eventLog.push({ t: Date.now(), focused: next, src });
    if (eventLog.length > EVENT_LOG_MAX) eventLog.splice(0, eventLog.length - EVENT_LOG_MAX);
  }
  set(next);
}

const teardown: UnlistenFn[] = [];

try {
  void listen<boolean>("app://focused", (e) => record(e.payload, "rust")).then((un) =>
    teardown.push(un),
  );
  void getCurrentWindow()
    .onFocusChanged(({ payload }) => record(payload, "tauri"))
    .then((un) => teardown.push(un));
  if (import.meta.env.DEV) {
    (window as unknown as { __focus?: object }).__focus = {
      state: () => focused,
      events: () => eventLog.slice(-10),
    };
  }
} catch {
  // Not inside Tauri (plain-browser dev) - stays "focused".
}

/** Exposed so HMR / tests can drop the module-scope subscriptions. */
export function disposeWindowFocus(): void {
  while (teardown.length) teardown.pop()?.();
  listeners.clear();
}

if (import.meta.hot) {
  import.meta.hot.dispose(() => disposeWindowFocus());
}

function subscribe(cb: () => void): () => void {
  listeners.add(cb);
  return () => {
    listeners.delete(cb);
  };
}

const getSnapshot = () => focused;

export function useWindowFocused(): boolean {
  // getServerSnapshot is supplied so the hook is safe under any
  // prerender/hydration path a future build setup might introduce.
  return useSyncExternalStore(subscribe, getSnapshot, getSnapshot);
}
