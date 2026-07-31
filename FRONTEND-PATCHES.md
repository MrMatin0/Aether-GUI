# Component-level patches (.tsx / config)

Shipped as complete replacement files:

- src-tauri/src/aether/mod.rs
- src-tauri/src/aether/pty.rs
- src-tauri/src/events.rs
- src-tauri/Cargo.toml
- src/types/connection.ts
- src/state/connectionStore.ts
- src/state/windowFocus.ts

The .tsx components need surgical edits, not rewrites.

---

## 1. AccessCodePrompt.tsx - stop deriving state from the log ring

Bug. Visibility is computed by counting a marker string inside logs:

    const promptCount = useMemo(
      () => logs.filter((l) => l.line === MARKER).length,
      [logs],
    );
    const waiting = promptCount > submittedFor;

Three failures fall out of that:

1. logs is a 500-entry ring (slice(-MAX_LOG_LINES)). Aether emits hundreds of
   lines while scanning. Once the marker is evicted, promptCount drops to 0,
   waiting flips false, and the code field DISAPPEARS while the user is still
   typing - with Aether still blocked on stdin, so the attempt then dies on
   the connect timeout.
2. submittedFor is component state that never resets. On the second prompt of
   a session (rejected code, or an auto-retry) promptCount restarts at 1 while
   submittedFor is already 1, so the field never reappears.
3. It is an O(500) filter re-run on every log flush (10x/sec) in an
   always-mounted component.

Fix. Drop the logs subscription, the useMemo, and submittedFor. Use the new
authoritative counter fed by the aether://access-code event:

    const waiting = useAccessCodePending();
    const submitAccessCode = useConnectionStore((s) => s.submitAccessCode);
    const [error, setError] = useState<string | null>(null);

    const submit = async (event: FormEvent) => {
      event.preventDefault();
      if (!code.trim() || sending) return;
      setSending(true);
      setError(null);
      try {
        await submitAccessCode(code);
        setCode('');
      } catch (e) {
        setError(toAetherError(e).message);
      } finally {
        setSending(false);
      }
    };

Note the added catch: the original had try/finally with no catch, so a
rejected invoke cleared the field, produced an unhandled rejection, and left
the user believing the code was submitted.

---

## 2. AdvancedPanel.tsx - the log panel is the biggest render cost in the app

A. Index keys over a ring buffer. logs.map((l, i) => key={i}) combined with
slice(-500) means that once the buffer is full, EVERY row changes identity on
every flush. React re-reconciles 500 nodes ten times a second for the whole
duration of a scan. The store now attaches a monotonic id: use key={l.id} and
hoist the row into a memo() component.

B. Synchronous layout thrash. The autoscroll effect reads scrollHeight
10x/sec, forcing a layout each time. Defer it and skip when collapsed:

    useEffect(() => {
      if (!open || !autoScroll) return;
      const el = viewportRef.current;
      if (!el) return;
      const raf = requestAnimationFrame(() => { el.scrollTop = el.scrollHeight; });
      return () => cancelAnimationFrame(raf);
    }, [logs, autoScroll, open]);

C. The panel subscribes to logs while collapsed. Radix unmounts
CollapsibleContent, but AdvancedPanel itself holds
useConnectionStore((s) => s.logs), so it re-renders 10x/sec with the
disclosure shut - which is the default state, i.e. the common case. Move the
logs subscription and the autoscroll effect down into a LogsPanel child
rendered inside CollapsibleContent. Highest-value frontend change here:
collapsed, the log stream should cost the React tree zero renders.

D. locked is derived here but each child toggle re-derives it from status.
Pass locked down as a required disabled: boolean prop so the lock rule lives
in exactly one place.

---

## 3. ConnectionStatusLine.tsx - the session timer never sleeps

Bug. useElapsed runs setInterval(..., 1000) for as long as sinceMs is set.
While Connected that is forever, including minimised to tray. The entire
design premise (every looping animation freezes while the window is unfocused,
so the app costs next to nothing in the background) is undone by a 1Hz React
re-render that wakes the WebView2 renderer and repaints text nobody can see.

    function useElapsed(sinceMs: number | null) {
      const focused = useWindowFocused();
      const [now, setNow] = useState(() => Date.now());
      useEffect(() => {
        if (sinceMs == null) return;
        setNow(Date.now());   // resync immediately on refocus
        if (!focused) return; // ...then stop ticking while hidden
        const id = setInterval(() => setNow(Date.now()), 1000);
        return () => clearInterval(id);
      }, [sinceMs, focused]);
      /* unchanged formatting */
    }

Also: this hook is invoked twice in the component, so it owns two independent
intervals. The fix above makes both free while hidden.

Context: attemptStartedAt, scanPercent and ScanProgressBar all render only in
the Connecting branch - which, before the backend fix in mod.rs, was
UNREACHABLE. See R1 in AUDIT.md.

---

## 4. App.tsx - effect cleanup races StrictMode

    useEffect(() => {
      const cleanup = initConnectionListeners();
      return () => { void cleanup.then((unlisten) => unlisten()); };
    }, []);

Under React 19 StrictMode the effect body runs twice before the first promise
resolves, so two independent listen() pairs register against the same store
and every log line is delivered twice in dev. The rewritten
initConnectionListeners de-duplicates behind a module-level promise, so
App.tsx needs no change - keep it as is.

---

## 5. ConnectButton.tsx

phaseOf() maps Reconnecting into the connecting phase, so clicking routes to
disconnect(). On the old backend that rejected with not_connected, because
there is no live session mid-backoff. The rewritten request_disconnect handles
the sessionless case explicitly, so cancel-during-reconnect now works. No
component change required - noted because the two halves only make sense
together.

Nit: RING_ANIM.error is an empty string, which yields a trailing space through
cn(). Use undefined.

---

## 6. src-tauri/tauri.conf.json

CSP has no connect-src. Tauri v2 serves IPC over ipc://localhost (and
http://ipc.localhost on Windows); today it works only because Tauri rewrites
the policy at runtime. Be explicit and tighten the rest:

    default-src 'self';
    connect-src 'self' ipc: http://ipc.localhost;
    style-src 'self' 'unsafe-inline';
    img-src 'self' data:;
    object-src 'none';
    base-uri 'none';
    frame-ancestors 'none'

Also: bundle.targets is set to all in a Windows-only shipping repo, so the
bundler attempts targets you never publish. Pin it to nsis + msi and let the
CI matrix override per OS.

---

## 7. src/index.css (Tailwind v4)

Mostly clean and genuinely well reasoned. Two notes:

- The universal selector rule in @layer base applies a border-color and an
  outline-color to every element in the tree. It is the stock shadcn snippet,
  but on a 420x640 window with a live log list it is thousands of extra
  declarations to match. Scope it to the elements that need it.
- The comment references LogsPanel.tsx, which does not exist in the tree - the
  log viewer currently lives inline in AdvancedPanel.tsx. Patch 2C extracts it
  and makes the comment true again.
