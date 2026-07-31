use serde::ser::{Serialize, SerializeStruct, Serializer};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AetherError {
    #[error("Aether is already running")]
    AlreadyRunning,
    #[error("Aether binary not found at {0}")]
    BinaryMissing(String),
    #[error("failed to launch Aether: {0}")]
    SpawnFailed(String),
    #[error("port {0} is already in use by another process")]
    PortInUse(u16),
    #[error("no active connection")]
    NotConnected,
    #[error("internal error: {0}")]
    Internal(String),
}

impl AetherError {
    /// Stable machine-readable discriminant. The frontend branches on this
    /// instead of substring-matching the human-readable message, so rewording
    /// a `#[error]` string can never silently break UI routing.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::AlreadyRunning => "already_running",
            Self::BinaryMissing(_) => "binary_missing",
            Self::SpawnFailed(_) => "spawn_failed",
            Self::PortInUse(_) => "port_in_use",
            Self::NotConnected => "not_connected",
            Self::Internal(_) => "internal",
        }
    }
}

impl Serialize for AetherError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("AetherError", 2)?;
        s.serialize_field("code", self.code())?;
        s.serialize_field("message", &self.to_string())?;
        s.end()
    }
}
