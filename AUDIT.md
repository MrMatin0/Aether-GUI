# Aether-GUI - code audit and refactor

Scope: full read of `src-tauri/src/**` and `src/**` on `MrMatin0/Aether-GUI@main`
(fork of `MatinSenPai/Aether-GUI`, v0.7.0).

Overall: this is a well-built codebase. The comments are unusually good - they
record *why*, and several of them document real production incidents. The PTY
line-draining logic, the decision to treat a live SOCKS5 port as ground truth
rather than trusting log wording, and keeping Zero Trust secrets out of argv
are all correct calls that most projects get wrong.

The problems below are concentrated in three places: the state machine has a
transition that can never fire, the concurrency model has no epoch guard, and
the frontend derives durable state from a bounded log buffer.

---

# CRITICAL

## R1 - the `Connecting` state is unreachable, so all live-progress UI is dead code

`pty.rs` sets `prompts_done` only here:

    answered.insert(section);
    if answered.len() == PROMPT_TABLE.len() {
        prompts_done.store(true, Ordering::Relaxed);
    }

All four `PROMPT_TABLE` rules must be answered. But the whole point of
`ConnectionProfile::as_args()` is that Aether >= 1.1.1 takes the entire profile
as CLI flags, and `AETHER_MASQUE_HTTP2` suppresses the fourth prompt outright.
In the normal path Aether prints **no interactive prompts at all**, so
`answered` stays empty and the flag never flips.

`monitor_connect` gates its `Launching -> Connecting` transition on exactly
that flag, so the app goes `Launching -> Connected` and never passes through
`Connecting`. Everything downstream of it in `ConnectionStatusLine.tsx` -
the elapsed timer, the scan percentage, `ScanProgressBar` - renders only in
the `Connecting` branch. The README advertises real elapsed time and a real
progress bar; in practice a Thorough scan sits on
`Starting Aether... / Answering setup prompts` for up to five and a half
minutes with no motion.

Fix: prompt completion is now a fast path, not the only path. `monitor_connect`
advances on `signals.prompts_done() || started.elapsed() >= PROMPT_PHASE_GRACE`
(2s). The reader thread also sets the flag on EOF so nothing can wait on it
forever.

## R2 - no epoch guard: cancel-then-reconnect spawns two Aether processes

Sequence, all reachable by clicking:

1. Connection drops. `handle_unexpected_failure` sets `Reconnecting` and
   spawns a thread that sleeps up to 10s.
2. User hits Cancel. `request_disconnect` sees `session.is_none()`, sets
   `user_requested_stop = true`, emits `Idle`, returns. The sleeping thread is
   untouched.
3. User hits Connect. `start_connect` passes the state guard (`Idle`) and
   calls `spawn_and_monitor`, which does `mgr.user_requested_stop = false`.
4. The old retry thread wakes, re-reads the now-cleared flag, and calls
   `spawn_and_monitor` itself.

Result: two live `aether` children. The second write to `mgr.session = Some(..)`
drops the first `PtySession` **without ever calling `kill()`**, so process one
is orphaned, still holding the SOCKS port, and the pid file points at the wrong
process. The next launch's `reap_orphan` cannot clean it up.

The same shape applies to the stale `monitor_connect` / `monitor_connected`
threads, which keep polling `try_wait()` against whatever session happens to be
in the manager and can drive state transitions for a lineage that no longer
exists.

Fix: `AetherManager::generation`, a monotonic epoch bumped by `start_connect`,
`request_disconnect` and `shutdown_blocking`. Every background thread captures
its generation at birth and no-ops the moment it stops matching. All state
writes go through `set_state_if_current`, which is a no-op for a superseded
lineage. `spawn_and_monitor` also re-checks the generation after spawning and
kills the child immediately if it lost the race.

## R3 - `String(e)` destroys the typed error contract

`error.rs` goes out of its way to serialise a stable discriminant:

    s.serialize_field(code, self.code())?;
    s.serialize_field(message, &self.to_string())?;

with a doc comment explaining that the frontend branches on `code` so that
rewording a message can never break UI routing. Then `connectionStore.ts` does:

    const message = String(e);
    if (message.toLowerCase().includes('binary not found')) { ... }

`e` is the deserialised object `{ code, message }`. `String(e)` on a plain
object is `[object Object]`. So:

- the substring test never matches - `SidecarErrorScreen` is **unreachable**,
  and a missing `aether.exe` (the single most likely install failure) shows as
  a generic error instead of the dedicated recovery screen;
- every connect failure renders the literal text `[object Object]` in the
  status line, because that string is written straight into
  `status.message` and `ConnectionStatusLine` prints it verbatim.

As a bonus, the substring being searched for (`binary not found`) does not even
match the actual message (`Aether binary not found at ...`) case-insensitively
by accident - it does match, but only because of the lowercase call. Fragile
either way.

Fix: `toAetherError()` in `types/connection.ts` normalises any rejection into
`{ code, message }`, and the store branches on `code === 'binary_missing'`
(plus `spawn_failed`, which is equally unrecoverable from the UI).

## R4 - the menu answerer can type a protocol digit into the Cloudflare code prompt

`looks_like_choice_prompt()` is literally `partial.trim_end().ends_with(':')`.
The Zero Trust prompt is `Enter the code:`. It ends with a colon. In `read_loop`
the access-code branch only *logs*; execution then falls straight into the
answering branch, and if any `current_section` is armed and unanswered, the GUI
writes `2\r\n` into Cloudflare Access's one-time-code field. That burns the code
and the enrolment attempt, and the user sees nothing explaining why.

Fix: `continue` after signalling the access-code prompt - the menu answerer is
never reachable while a code prompt is on screen.

## R5 - every force-killed child becomes a zombie

Three call sites do `session.kill()` then `mgr.session = None`. `portable-pty`'s
Unix child does not reap on `Drop`, so the process stays in the table as a
defunct entry. The app auto-retries three times and each timeout path kills,
so a bad network produces a steady drip of zombies for the lifetime of the GUI.

Fix: `PtySession::kill_and_reap(grace)` - kill, poll `try_wait` briefly, then
fall back to a blocking `wait()` (which returns instantly, the signal is
already delivered). Used at every kill site.

---

# HIGH

## R6 - a prompt header stays armed forever

`current_section` is set by any completed line whose suffix matches a header,
and is never cleared. Combined with R4's extremely loose prompt heuristic, any
colon-terminated partial line at any later point in the session - including
hours into an established tunnel - can inject a stray keystroke into Aether's
stdin. This is a live PTY writing into a running tunnel process.

Fix: a four-line arming window (`lines_since_header <= HEADER_ARM_WINDOW`). A
header authorises answering the prompt that immediately follows it, nothing
else.

## R7 - split UTF-8 sequences are silently corrupted

    line_buf.push_str(&String::from_utf8_lossy(&byte_buf[..n]));

`from_utf8_lossy` is applied per 4096-byte read. A multi-byte character
straddling a read boundary is permanently replaced with U+FFFD. That mangles
Aether's box-drawing output and any non-ASCII string (this project ships a
Persian README and targets a Persian-speaking user base), and - worse - a
corrupted byte inside a header line means `header_matches` never fires, which
is exactly the infinite Protocol/Scan-mode loop `prompts.rs` documents as an
observed production incident.

Fix: carry the incomplete trailing sequence across reads and decode
incrementally with `str::from_utf8` + `Utf8Error::valid_up_to`/`error_len`.
Regression test included.

Related: `strip_ansi` only understood CSI (`ESC [ ... letter`). An OSC sequence
(`ESC ] 0 ; title BEL`, which is how a terminal app sets the window title)
leaked its entire payload into the log panel and into the prompt heuristic. Now
handled, along with two-character escapes.

## R8 - a 300ms blocking TCP connect is performed while holding the global mutex

`monitor_connect` takes `manager.lock()` at the top of each iteration and holds
it across `status::port_is_live(&socks)` - a `TcpStream::connect_timeout` with a
300ms budget - on a 400ms loop. During the entire connect phase the manager
mutex is unavailable roughly 40% of the time, and `get_status`, `disconnect` and
`submit_access_code` all block on it. That is why cancelling during a scan feels
laggy. `start_connect` does the same thing on the IPC thread.

Fix: the loop is split into three phases - a short locked read for `try_wait`,
a completely lock-free check of the reader-thread atomics, then the blocking
probe outside the lock. `start_connect` probes before taking the lock; the
state check under the lock still serialises a double-click, which is what that
guard actually exists for.

## R9 - `.lock().unwrap()` everywhere turns any panic into a permanently bricked app

Every access is `manager.lock().unwrap()`. One panic in any thread holding the
mutex poisons it forever, after which every Tauri command - including
`get_status` - panics on invocation and the window is a frozen shell the user
cannot even close cleanly.

Fix: a single `lock()` helper using `unwrap_or_else(|p| p.into_inner())`. The
invariants the mutex protects are re-established by the generation guard, so
recovering is strictly better than dying.

## R10 - the Zero Trust code prompt is derived from a 500-entry ring buffer

`AccessCodePrompt` decides whether to render by counting a marker line inside
`logs`, which is capped at 500 entries. Aether emits hundreds of lines during a
scan. When the marker is evicted the prompt vanishes mid-typing while the tunnel
process is still blocked on stdin, and the attempt dies on the connect timeout.
The local `submittedFor` counter also never resets, so the second prompt of any
session never appears.

Fix: a dedicated `aether://access-code` event carrying a monotonic `sequence`,
plus `accessCodeRequested` / `accessCodeAnswered` in the store and a
`useAccessCodePending()` selector. Durable UI state should never be inferred
from a lossy log stream.

## R11 - a dead tunnel with a live process reports Connected forever

`monitor_connected` only watches for process exit. If the tunnel collapses but
the `aether` process stays up (entirely possible: the SOCKS listener and the
supervisor are the same binary but not the same failure domain), the GUI keeps
reporting `Connected` over a proxy that refuses every connection. That is
precisely the scenario the auto-reconnect feature exists to hide, and it is the
one case it cannot see.

Fix: a 5s health probe of the SOCKS port while `Connected`, two consecutive
failures before declaring a drop, feeding the existing retry path.

---

# PERFORMANCE

## P1 - one IPC message per log line

The Rust side emitted `aether://log` per line while the frontend was already
coalescing arrivals into a 100ms window - so the extra traffic bought exactly
nothing and cost a serde round trip plus a webview main-thread task each. A
Thorough scan produces thousands. Now batched at ~120ms / 256 lines on the Rust
side into a `LogBatch`, with the access-code signal flushed immediately ahead of
the queue so a human waiting on an email code is not delayed.

## P2 - the log list re-reconciles 500 DOM nodes ten times a second

`logs.map((l, i) => key={i})` over a `slice(-500)` ring means every row changes
identity on every flush once the buffer is full. Store rows now carry a
monotonic `id`. See FRONTEND-PATCHES.md sections 2A-2C; the bigger win is 2C -
moving the `logs` subscription inside `CollapsibleContent` so the panel costs
zero renders in its default collapsed state.

## P3 - the store allocated two arrays per flush

`[...s.logs, ...batch].slice(-500)` allocates a full copy plus a slice, 10x/sec,
for the duration of a scan. Replaced with a conditional `slice().concat()` that
only copies when the buffer actually overflows.

## P4 - the session timer keeps the renderer awake in the tray

`useElapsed` ticks at 1Hz for as long as the app is `Connected`, including
minimised. The rest of the app is meticulous about pausing every animation on
focus loss; this one React re-render undoes it. Gate on `useWindowFocused()`
and resync on refocus - see FRONTEND-PATCHES.md section 3.

## P5 - no release profile

`Cargo.toml` had no `[profile.release]` at all: opt-level 3, 16 codegen units,
no LTO, full symbols, unwinding panics. Added `opt-level = s`, fat LTO, one
codegen unit, `strip`, `panic = abort`. Typically 30-45% off the Windows binary
and a measurably faster cold start, which matters for something people launch
and immediately click once.

---

# MEMORY / LIFECYCLE

## M1 - unbounded focus event log

`windowFocus.ts` pushes an entry into `eventLog` on every focus change and never
trims it, while only ever reading the last ten. In a tray-resident app the user
alt-tabs past dozens of times an hour, that array grows for the entire uptime of
the process. Now a 20-entry bounded ring, DEV-only.

## M2 - module-scope listeners with no teardown

`windowFocus.ts` registers two Tauri listeners at import time and drops the
returned `UnlistenFn`s on the floor. Fine for app lifetime, but it leaks a new
pair on every Vite HMR cycle in dev. Now retained, with an
`import.meta.hot.dispose` teardown.

## M3 - StrictMode double-registers every IPC listener

`initConnectionListeners()` is called from an effect whose cleanup awaits the
promise. Under React 19 StrictMode the effect body runs twice before the first
promise resolves, so two independent listener sets attach to the same store and
every log line arrives twice in dev. Now de-duplicated behind a module-level
promise.

---

# HARDENING (no bug, worth doing)

- **Inherited proxy environment.** Aether inherits the GUI's env, and users on
  censored networks very often already have `HTTP_PROXY` / `ALL_PROXY` exported.
  Aether's own route probes would then be tunnelled through the broken path they
  are trying to escape. `pty.rs` now strips the six usual variables at spawn.
- **CSP has no `connect-src`.** Tauri v2 IPC rides `ipc://localhost` /
  `http://ipc.localhost`. It works today only because Tauri rewrites the policy
  at runtime. Be explicit, and add `object-src none` / `base-uri none` /
  `frame-ancestors none`.
- **Dependency drift.** `thiserror 1` -> `2`, `portable-pty 0.8` -> `0.9`.
- **`bundle.targets: all`** in a Windows-only shipping repo. Pin to nsis + msi.
- **`orphan.rs` shells out** to `kill` / `tasklist` / `taskkill`. It works, but
  `tasklist /FI` substring-matching the pid against the whole output can false
  positive on an unrelated process whose memory figure contains the digits. On
  Windows use `OpenProcess` + `GetExitCodeProcess`; on Unix use `libc::kill(pid, 0)`.
  Also: a recycled pid means this can kill an innocent process. Storing the
  process start time alongside the pid removes that class of bug entirely.
- **`AetherManager::new()` with a manual constructor** - clippy's
  `new_without_default`. Added `Default`.

---

# What I deliberately did not change

- The PTY line-draining semantics in `drain_lines`. It correctly models CR
  overwrite versus CRLF including the ONLCR double-CR case and the split-CRLF
  boundary, it is tested, and the reasoning in the comment is right.
- Treating a live SOCKS5 port as ground truth for connected. Correct, and the
  `0.0.0.0` -> `127.0.0.1` probe rewrite is a nice touch.
- Keeping Zero Trust credentials in env vars rather than argv, and scrubbing
  them before persisting the profile. Both correct.
- The CSS-over-Motion animation strategy in `index.css`. The comments describe
  real measurements and the conclusion is right.
- The `focus.rs` `GetForegroundWindow` poller. Ugly, and correct - the reasons
  listed in its doc comment are all genuine WebView2 behaviours.
