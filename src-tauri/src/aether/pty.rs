use super::profiles::{ConnectionProfile, ZeroTrustAuth};
use super::prompts::{looks_like_choice_prompt, PROMPT_TABLE};
use crate::error::AetherError;
use crate::events::{now_millis, LogEvent};
use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use std::collections::HashSet;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Everything the supervisor needs to observe about a live PTY without
/// touching the child handle (which is `&mut`-only and lives behind the
/// global manager lock).
#[derive(Debug, Default)]
pub struct SessionSignals {
    prompts_done: AtomicBool,
    lines_seen: AtomicU64,
    access_code_seq: AtomicU64,
}

impl SessionSignals {
    pub fn prompts_done(&self) -> bool {
        self.prompts_done.load(Ordering::Acquire)
    }
    pub fn lines_seen(&self) -> u64 {
        self.lines_seen.load(Ordering::Relaxed)
    }
    pub fn access_code_seq(&self) -> u64 {
        self.access_code_seq.load(Ordering::Acquire)
    }
}

/// Outbound signal from the reader thread that isn't a log line.
pub enum PtySignal {
    Log(LogEvent),
    AccessCodeRequested { sequence: u64 },
}

pub struct PtySession {
    child: Box<dyn Child + Send + Sync>,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    signals: Arc<SessionSignals>,
    /// Keeps the pty master (and thus the slave/child's controlling tty) alive
    /// for the life of the session; never read from directly after spawn.
    _master: Box<dyn MasterPty + Send>,
}

impl PtySession {
    pub fn pid(&self) -> u32 {
        self.child.process_id().unwrap_or(0)
    }

    pub fn signals(&self) -> Arc<SessionSignals> {
        Arc::clone(&self.signals)
    }

    pub fn prompts_done(&self) -> bool {
        self.signals.prompts_done()
    }

    pub fn try_wait(&mut self) -> Option<portable_pty::ExitStatus> {
        self.child.try_wait().ok().flatten()
    }

    /// Ctrl-C (ETX) — the same byte a real terminal sends for SIGINT.
    pub fn send_ctrl_c(&self) {
        if let Ok(mut w) = self.writer.lock() {
            let _ = w.write_all(&[0x03]);
            let _ = w.flush();
        }
    }

    /// Feeds the one-time code requested by Cloudflare Access during a Zero
    /// Trust email enrolment. The code never enters the log stream.
    pub fn send_access_code(&self, code: &str) -> Result<(), AetherError> {
        let code = code.trim();
        if code.is_empty() || code.len() > 512 || code.contains(['\r', '\n']) {
            return Err(AetherError::Internal(
                "invalid Zero Trust access code".into(),
            ));
        }
        let mut writer = self
            .writer
            .lock()
            .map_err(|_| AetherError::Internal("Aether input is unavailable".into()))?;
        writer
            .write_all(code.as_bytes())
            .and_then(|_| writer.write_all(b"\r\n"))
            .and_then(|_| writer.flush())
            .map_err(|e| AetherError::Internal(format!("sending Zero Trust access code: {e}")))
    }

    pub fn kill(&mut self) {
        let _ = self.child.kill();
    }

    /// BUGFIX (R5): the previous code called `kill()` and then dropped the
    /// `Child`. `portable_pty`'s Unix child does not reap on drop, so every
    /// force-killed Aether left a zombie behind for the lifetime of the GUI —
    /// one per failed attempt, and this app auto-retries three times. Always
    /// reap after killing.
    pub fn kill_and_reap(&mut self, grace: Duration) {
        let _ = self.child.kill();
        let deadline = Instant::now() + grace;
        loop {
            match self.child.try_wait() {
                Ok(Some(_)) | Err(_) => return,
                Ok(None) => {}
            }
            if Instant::now() >= deadline {
                // Last resort: a blocking wait. The signal has already been
                // delivered, so this returns essentially immediately.
                let _ = self.child.wait();
                return;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
    }
}

/// Spawns Aether in a real PTY (not a plain piped subprocess) and answers its
/// known interactive prompts as they appear.
pub fn spawn(
    binary: &Path,
    cwd: &Path,
    profile: ConnectionProfile,
    tx: Sender<PtySignal>,
) -> Result<PtySession, AetherError> {
    let pty_system = native_pty_system();
    let pair = pty_system
        .openpty(PtySize {
            rows: 40,
            cols: 120,
            pixel_width: 0,
            pixel_height: 0,
        })
        .map_err(|e| AetherError::SpawnFailed(e.to_string()))?;

    let mut cmd = CommandBuilder::new(binary);
    cmd.cwd(cwd);
    for arg in profile.as_args() {
        cmd.arg(arg);
    }
    cmd.env(
        "AETHER_MASQUE_HTTP2",
        if profile.masque_http2 { "1" } else { "0" },
    );
    // HARDENING (R9): Aether inherits the GUI's environment. Users on
    // censored networks very often already have a system proxy exported, and
    // Aether's own route probes would then be tunnelled through it — which is
    // exactly the broken path they are trying to escape. Strip the usual
    // suspects so probing always measures the raw network.
    for var in [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ] {
        cmd.env_remove(var);
    }
    // Keep Access credentials out of the process command line.
    match profile.zero_trust_auth {
        ZeroTrustAuth::Service
            if !profile.access_client_id.trim().is_empty()
                && !profile.access_client_secret.trim().is_empty() =>
        {
            cmd.env("AETHER_ACCESS_CLIENT_ID", profile.access_client_id.trim());
            cmd.env(
                "AETHER_ACCESS_CLIENT_SECRET",
                profile.access_client_secret.trim(),
            );
        }
        _ => {
            if let Some((key, value)) = profile.zero_trust_env() {
                cmd.env(key, value);
            }
        }
    }

    let child = pair
        .slave
        .spawn_command(cmd)
        .map_err(|e| AetherError::SpawnFailed(e.to_string()))?;
    drop(pair.slave);

    let mut reader = pair
        .master
        .try_clone_reader()
        .map_err(|e| AetherError::SpawnFailed(e.to_string()))?;

    let raw_writer = pair
        .master
        .take_writer()
        .map_err(|e| AetherError::SpawnFailed(e.to_string()))?;
    let writer = Arc::new(Mutex::new(raw_writer));
    let writer_for_thread = Arc::clone(&writer);

    let signals = Arc::new(SessionSignals::default());
    let signals_for_thread = Arc::clone(&signals);

    std::thread::Builder::new()
        .name("aether-pty-reader".into())
        .spawn(move || {
            read_loop(
                reader.as_mut(),
                writer_for_thread,
                profile,
                tx,
                signals_for_thread,
            );
        })
        .map_err(|e| AetherError::SpawnFailed(e.to_string()))?;

    Ok(PtySession {
        child,
        writer,
        signals,
        _master: pair.master,
    })
}

/// The literal Aether emits when it wants a Cloudflare Access one-time code.
const ACCESS_CODE_PROMPT: &str = "Enter the code:";

fn read_loop(
    reader: &mut dyn Read,
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    profile: ConnectionProfile,
    tx: Sender<PtySignal>,
    signals: Arc<SessionSignals>,
) {
    let mut answered: HashSet<&'static str> = HashSet::new();
    let mut current_section: Option<&'static str> = None;
    // BUGFIX (R6): a header only authorises answering the prompt that
    // immediately follows it. Previously a header stayed "armed" forever, so
    // any later partial line ending in ':' — including Aether's own
    // colon-terminated info logs mid-tunnel — could inject a stray menu digit
    // into the running process's stdin.
    let mut lines_since_header: u32 = 0;
    const HEADER_ARM_WINDOW: u32 = 4;

    let mut line_buf = String::new();
    // BUGFIX (R7): the old loop did `String::from_utf8_lossy(&byte_buf[..n])`
    // per read. A multi-byte character straddling a 4096-byte read boundary
    // was permanently replaced with U+FFFD, corrupting any non-ASCII output
    // (Aether's box-drawing characters and the Persian build's strings) and,
    // worse, potentially corrupting a prompt header so it never matched.
    // Carry the incomplete tail across reads instead.
    let mut carry: Vec<u8> = Vec::with_capacity(4);
    let mut byte_buf = [0u8; 8192];
    let mut code_prompt_visible = false;
    let mut access_code_seq: u64 = 0;

    loop {
        let n = match reader.read(&mut byte_buf) {
            Ok(0) => break, // EOF: process exited or pty closed
            Ok(n) => n,
            Err(_) => break,
        };
        carry.extend_from_slice(&byte_buf[..n]);
        let valid_up_to = match std::str::from_utf8(&carry) {
            Ok(s) => {
                line_buf.push_str(s);
                carry.len()
            }
            Err(e) => {
                let good = e.valid_up_to();
                // SAFETY-free equivalent: re-validate the good prefix.
                line_buf.push_str(std::str::from_utf8(&carry[..good]).unwrap_or_default());
                match e.error_len() {
                    // Genuinely invalid bytes: emit one replacement char and
                    // skip them, otherwise we would stall forever.
                    Some(bad) => {
                        line_buf.push('\u{fffd}');
                        good + bad
                    }
                    // Truncated sequence: keep it for the next read.
                    None => good,
                }
            }
        };
        carry.drain(..valid_up_to);

        for raw_line in drain_lines(&mut line_buf) {
            let line = strip_ansi(&raw_line);
            if line.is_empty() {
                continue;
            }
            let mut matched_header = false;
            for rule in PROMPT_TABLE {
                if (rule.header_matches)(&line) {
                    current_section = Some(rule.id);
                    matched_header = true;
                    // Seeing a header again means Aether restarted its prompt
                    // sequence — allow re-answering, or it blocks forever.
                    answered.remove(rule.id);
                }
            }
            lines_since_header = if matched_header {
                0
            } else {
                lines_since_header.saturating_add(1)
            };
            signals.lines_seen.fetch_add(1, Ordering::Relaxed);
            let _ = tx.send(PtySignal::Log(LogEvent {
                line,
                timestamp: now_millis(),
            }));
        }

        let partial = strip_ansi(&line_buf);

        // Aether 1.5.0 asks for the Cloudflare Access one-time code without a
        // terminating newline, so normal line forwarding cannot expose it.
        let access_code_prompt = partial.contains(ACCESS_CODE_PROMPT);
        if access_code_prompt && !code_prompt_visible {
            access_code_seq += 1;
            signals
                .access_code_seq
                .store(access_code_seq, Ordering::Release);
            let _ = tx.send(PtySignal::AccessCodeRequested {
                sequence: access_code_seq,
            });
            let _ = tx.send(PtySignal::Log(LogEvent {
                line: "[gui] Zero Trust access code required".into(),
                timestamp: now_millis(),
            }));
        }
        code_prompt_visible = access_code_prompt;

        // BUGFIX (R4): `looks_like_choice_prompt` is "ends with ':'", and the
        // access-code prompt also ends with ':'. The old order let the menu
        // answerer fire on it and type a protocol digit into Cloudflare's
        // one-time-code field, burning the code and the enrolment attempt.
        if access_code_prompt {
            continue;
        }

        if lines_since_header <= HEADER_ARM_WINDOW
            && looks_like_choice_prompt(&partial)
            && !PROMPT_TABLE.iter().any(|r| (r.header_matches)(&partial))
        {
            if let Some(section) = current_section {
                if !answered.contains(section) {
                    if let Some(rule) = PROMPT_TABLE.iter().find(|r| r.id == section) {
                        let answer = (rule.answer)(&profile);
                        if let Ok(mut w) = writer.lock() {
                            let _ = w.write_all(answer.as_bytes());
                            let _ = w.write_all(b"\r\n");
                            let _ = w.flush();
                        }
                        let _ = tx.send(PtySignal::Log(LogEvent {
                            line: format!("[gui] answered {section} \u{2192} {answer}"),
                            timestamp: now_millis(),
                        }));
                        answered.insert(section);
                        if answered.len() == PROMPT_TABLE.len() {
                            signals.prompts_done.store(true, Ordering::Release);
                        }
                    }
                }
            }
        }
    }

    // EOF: whatever prompts were or weren't answered, nothing more is coming.
    // Release any supervisor waiting on this flag.
    signals.prompts_done.store(true, Ordering::Release);
}

/// Longest the unterminated tail may grow before the front is discarded.
const MAX_PARTIAL: usize = 16 * 1024;

/// Drains and returns every terminated line in `buf`, leaving the
/// unterminated tail in place. Terminal semantics, not plain `\n`-splitting.
fn drain_lines(buf: &mut String) -> Vec<String> {
    let mut lines = Vec::new();
    while let Some(pos) = buf.find(['\r', '\n']) {
        let end = if buf.as_bytes()[pos] == b'\n' {
            pos
        } else {
            let mut run_end = pos;
            while run_end < buf.len() && buf.as_bytes()[run_end] == b'\r' {
                run_end += 1;
            }
            if run_end == buf.len() {
                break; // "\r" at buffer end: might be a split "\r\n"
            }
            if buf.as_bytes()[run_end] != b'\n' {
                buf.drain(..run_end); // overwritten frame: discard silently
                continue;
            }
            run_end
        };
        let line: String = buf.drain(..=end).collect();
        lines.push(line.trim_end_matches(['\r', '\n']).to_string());
    }
    if buf.len() > MAX_PARTIAL {
        let mut cut = buf.len() - MAX_PARTIAL;
        while !buf.is_char_boundary(cut) {
            cut += 1;
        }
        buf.drain(..cut);
    }
    lines
}

/// Minimal ANSI stripper. Handles CSI (`ESC [ ... letter`) *and* OSC
/// (`ESC ] ... BEL | ESC \\`) — the previous version only knew CSI, so an OSC
/// title-set sequence leaked its whole payload into the log panel and, if it
/// happened to end in ':', into the prompt heuristic.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                for c2 in chars.by_ref() {
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(c2) = chars.next() {
                    if c2 == '\u{7}' {
                        break;
                    }
                    if c2 == '\u{1b}' && chars.peek() == Some(&'\\') {
                        chars.next();
                        break;
                    }
                }
            }
            // Two-character escapes (ESC ( B, ESC = , ...).
            Some(_) => {
                chars.next();
            }
            None => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(buf: &mut String, chunk: &str) -> Vec<String> {
        buf.push_str(chunk);
        drain_lines(buf)
    }

    #[test]
    fn plain_newlines() {
        let mut buf = String::new();
        assert_eq!(feed(&mut buf, "a\nb\nc"), ["a", "b"]);
        assert_eq!(buf, "c");
    }

    #[test]
    fn crlf_and_onlcr_double_cr() {
        let mut buf = String::new();
        assert_eq!(feed(&mut buf, "a\r\nb\r\r\n"), ["a", "b"]);
        assert_eq!(buf, "");
    }

    #[test]
    fn cr_overwrite_drops_spinner_frames() {
        let mut buf = String::new();
        assert_eq!(
            feed(&mut buf, "scan 1%\rscan 2%\rscan 3%"),
            Vec::<String>::new()
        );
        assert_eq!(buf, "scan 3%");
        assert_eq!(feed(&mut buf, "\rscan done\n"), ["scan done"]);
        assert_eq!(buf, "");
    }

    #[test]
    fn lone_cr_at_end_waits_for_possible_lf() {
        let mut buf = String::new();
        assert_eq!(feed(&mut buf, "abc\r"), Vec::<String>::new());
        assert_eq!(buf, "abc\r");
        assert_eq!(feed(&mut buf, "\n"), ["abc"]);
        assert_eq!(buf, "");
    }

    #[test]
    fn unterminated_tail_is_capped() {
        let mut buf = String::new();
        let big = "é".repeat(MAX_PARTIAL);
        assert_eq!(feed(&mut buf, &big), Vec::<String>::new());
        assert!(buf.len() <= MAX_PARTIAL + 1);
        assert!(buf.chars().all(|c| c == 'é'));
    }

    #[test]
    fn strips_csi_and_osc() {
        assert_eq!(strip_ansi("\u{1b}[32m[+]\u{1b}[0m ok"), "[+] ok");
        assert_eq!(strip_ansi("\u{1b}]0;Aether\u{7}ready"), "ready");
        assert_eq!(strip_ansi("\u{1b}]0;Aether\u{1b}\\ready"), "ready");
    }

    /// Regression for R7: a UTF-8 sequence split across two reads must not
    /// degrade to U+FFFD.
    #[test]
    fn split_utf8_across_reads_is_not_corrupted() {
        let text = "سلام\n";
        let bytes = text.as_bytes();
        let mut line_buf = String::new();
        let mut carry: Vec<u8> = Vec::new();
        let mut out = Vec::new();
        for chunk in bytes.chunks(3) {
            carry.extend_from_slice(chunk);
            let consumed = match std::str::from_utf8(&carry) {
                Ok(s) => {
                    line_buf.push_str(s);
                    carry.len()
                }
                Err(e) => {
                    let good = e.valid_up_to();
                    line_buf.push_str(std::str::from_utf8(&carry[..good]).unwrap());
                    good
                }
            };
            carry.drain(..consumed);
            out.extend(drain_lines(&mut line_buf));
        }
        assert_eq!(out, ["سلام"]);
    }
}
