use async_trait::async_trait;
use serde::{Deserialize, Serialize};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("authentication failed or requires a challenge")]
    Authentication,
    #[error("operation is unsupported: {0}")]
    Unsupported(String),
    #[error("transport error: {0}")]
    Transport(String),
    #[error("device rejected the request: HTTP {0}")]
    Http(u16),
    #[error("invalid device response: {0}")]
    InvalidResponse(String),
    #[error("input outcome is uncertain; do not replay automatically: {0}")]
    AmbiguousInput(String),
}

pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum DeviceProfile {
    PiKvm,
    GlKvm,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub info: serde_json::Value,
    pub hid: serde_json::Value,
    pub streamer: serde_json::Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Capability {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub status: Capability,
    pub snapshot: Capability,
    pub keyboard: Capability,
    pub mouse: Capability,
    pub live_video: Capability,
}

#[derive(Debug, Clone)]
pub struct Snapshot {
    pub jpeg: Vec<u8>,
}

/// A pull-based video stream. Dropping it releases its transport resources.
#[async_trait]
pub trait VideoStream: Send {
    /// Returns the next complete JPEG, or `None` when the stream ends normally.
    async fn next_frame(&mut self) -> Result<Option<Snapshot>>;
}

#[async_trait]
pub trait Device: Send + Sync {
    /// Opens a continuous video stream when supported by the device.
    async fn video(&self) -> Result<Box<dyn VideoStream>>;
    async fn status(&self) -> Result<DeviceStatus>;
    async fn capabilities(&self) -> Result<Capabilities>;
    async fn snapshot(&self) -> Result<Snapshot>;
    async fn key(&self, key: &str, pressed: bool) -> Result<()>;
    async fn mouse_button(&self, button: &str, pressed: bool) -> Result<()>;
    async fn mouse_move(&self, x: i16, y: i16) -> Result<()>;
    async fn release_inputs(&self) -> Result<()>;
}
