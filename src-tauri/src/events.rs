use serde::Serialize;

pub const STATUS_EVENT: &str = "aether://status";
pub const LOG_EVENT: &str = "aether://log";
/// Dedicated, non-log-derived signal that Aether is blocking on a Cloudflare
/// Access one-time code. The old design inferred this by counting a marker
/// line inside the rolling `logs` buffer on the frontend, which silently
/// broke as soon as the buffer rolled past 500 lines (see AUDIT.md F1).
pub const ACCESS_CODE_EVENT: &str = "aether://access-code";

#[derive(Serialize, Clone, Debug)]
pub struct LogEvent {
    pub line: String,
    /// Milliseconds since UNIX_EPOCH — avoids pulling in a date/time crate
    /// just to format a value the frontend can turn into a Date() itself.
    pub timestamp: u64,
}

/// Emitted as one message per ~120ms flush instead of one per line. A
/// Thorough scan produces thousands of lines; one IPC round-trip + one JSON
/// serialization each was measurable webview main-thread time for output the
/// user almost never opens.
#[derive(Serialize, Clone, Debug)]
pub struct LogBatch {
    pub lines: Vec<LogEvent>,
}

#[derive(Serialize, Clone, Debug)]
pub struct AccessCodeEvent {
    /// Monotonically increasing per PTY session. The frontend compares this
    /// against the last value it answered, so a rejected-then-reissued code
    /// prompt is unambiguous and never depends on log retention.
    pub sequence: u64,
    pub requested_at_ms: u64,
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
