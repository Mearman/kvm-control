use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Failure of a [`Device`] or [`VideoStream`] operation.
///
/// The variants divide by whether the device may have acted. When an input or control method (`key`, `mouse_button`, `mouse_move`, `release_inputs` or `control`) returns `Authentication`, `Unsupported`, `Transport`, `Http` or `InvalidResponse`, the input or control that call asked for was not submitted, so the caller may decide whether to try again; `kvm-kvmd` may still have sent releases owed to earlier interrupted or abandoned presses before failing. `AmbiguousInput` and `AmbiguousControl` returned by those methods mean the requested input or control was, or may have been, submitted and its effect is unknown: never replay either automatically, and observe the device and the controlled host before deciding what to do. `AmbiguousInput` from `login` or `complete_approval` is the exception: it refuses to change authentication while inputs are held, and sends nothing.
///
/// The string carried by a variant is a human-readable diagnostic, not a stable value to match on. The variant descriptions below say when the `kvm-kvmd` adapter returns each one; another [`Device`] implementation must keep the same division between unsubmitted and ambiguous outcomes.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The device refused the credentials or the session: an HTTP 401 or 403 response, or a login, approval or authentication check whose response did not report success.
    ///
    /// Authenticate again before retrying (in `kvm-kvmd`, `login`, or `login_with_totp` with a fresh code, followed by `complete_approval` while approval is pending). A refused login does not say whether a TOTP code is required. `kvm-kvmd`'s `login` discards the previous session before sending its request, once its local checks and the held-input check pass, so a login that then fails with any variant leaves the adapter unauthenticated. A session the device has expired or revoked also ends here. A refusal of an input or control request that was already submitted is reported as `AmbiguousInput` or `AmbiguousControl` instead, since the adapter cannot tell whether the device acted first.
    #[error("authentication failed or requires a challenge")]
    Authentication,
    /// The operation is not available on this connection, and the requested input or control was not submitted.
    ///
    /// `kvm-kvmd` returns it for a control operation whose capability discovery did not report support (an `Unknown` capability included), a switch port absent from the device's port inventory or an inventory that could not be confirmed, absolute mouse movement without an online absolute-mode mouse, device approval on a profile without an approval flow, and live video from a device whose stream route does not answer with multipart MJPEG (see `docs/video-transports.md` for the transports that are not implemented). It does not prove that the device lacks the feature, because unconfirmed support is reported the same way. Do not retry unchanged: read `Device::capabilities`, and check the selected profile and the device configuration.
    #[error("operation is unsupported: {0}")]
    Unsupported(String),
    /// A request failed in transport before the requested input or control was submitted, or a read-only transfer stopped.
    ///
    /// `kvm-kvmd` returns it for a connection failure or timeout, a response body that could not be read, a video stream lease that timed out, failed or closed, a video request or read that timed out, a video stream whose consumer did not ask for a frame in time, a JPEG validation task that failed, an HTTP client that could not be built, and a limit on concurrent snapshots or video stream storage being reached. Those limits are shared by every `Kvmd` adapter in the process, not held per adapter. A read-only operation can be retried after a delay; after a capacity limit, finish or drop other snapshots and video streams first. A video stream that returns it should be dropped and a new one opened. A transport failure of a submitted input or control request is reported as `AmbiguousInput` or `AmbiguousControl` instead.
    #[error("transport error: {0}")]
    Transport(String),
    /// The device answered with an unsuccessful HTTP status other than 401 or 403, carried as the status code, before the requested input or control was submitted.
    ///
    /// A 404 usually means the route is absent: a reverse-proxy prefix that does not forward it, a different profile or firmware without that subsystem, or a disabled service. Capability discovery maps a 404 from the route of an optional subsystem to an `Unsupported` or `Unknown` capability instead of returning it. A 5xx status reports a device or proxy failure; a read-only operation can be retried after a delay. An unsuccessful status for a submitted input or control request is reported as `AmbiguousInput` or `AmbiguousControl` instead.
    #[error("device rejected the request: HTTP {0}")]
    Http(u16),
    /// A device response was not one the adapter accepts, or the caller's own configuration or arguments were rejected.
    ///
    /// From the device, `kvm-kvmd` returns it for malformed or oversized JSON, a read that did not report success or lacked its result, a login or approval response missing a field the flow needs, an invalid or oversized JPEG, a malformed or oversized multipart stream, and a stream lease that was not upgraded to a WebSocket. Such a response points at firmware or a proxy the adapter does not support; retrying unchanged is unlikely to help. Locally it returns it, before contacting the device, for an endpoint that is not HTTPS or that contains credentials, a query or a fragment, an invalid CA certificate, credentials that cannot be sent as headers, Basic credentials containing a colon, a TOTP code that is not six ASCII digits, a GLKVM password and code longer than 63 bytes together, cookie login on an adapter built with header or Basic credentials, and `complete_approval` with no approval pending (checked after any owed input releases have been sent). Correct the configuration or call before trying again. A login answered with a two-step approval request under the PiKVM profile is also reported as this variant, but only after the credentials reached the device, which may have started an approval; select the GLKVM profile for a device that asks for approval.
    #[error("invalid device response: {0}")]
    InvalidResponse(String),
    /// A keyboard, mouse-button or mouse-movement request was, or may have been, submitted and its outcome was not confirmed; never replay it automatically.
    ///
    /// `kvm-kvmd` returns it for every failure of a submitted input request (a transport failure, an unsuccessful status including 401 and 403, an unreadable response or one that did not report success), for an input task that ended without an outcome, such as one interrupted by runtime shutdown, and from `release_inputs` while the input session is indeterminate. The device may still apply a timed-out request later, so the adapter instance's input session stays indeterminate for its lifetime: `release_inputs` keeps returning this variant, and even a confirmed later release cannot show that an earlier request will not arrive late. Observe the controlled host before sending further input, and await `release_inputs` and handle its error rather than assuming keys and buttons are up. `login` and `complete_approval` also return it while tracked inputs are still held, without sending their own request; this refusal is not ambiguous, so release the inputs and call again.
    #[error("input outcome is uncertain; do not replay automatically: {0}")]
    AmbiguousInput(String),
    /// A power, switch, GPIO or virtual media request was, or may have been, submitted and its effect is unknown; never replay it automatically.
    ///
    /// `kvm-kvmd` returns it when the submitted request fails in any way (a transport failure, an unsuccessful status including 401 and 403, an unreadable response or one that did not report success), and when the device acknowledged the request but the state read that should observe its effect failed. An acknowledged request whose effect could not be confirmed is not an error: it returns `Ok(ControlOutcome::Unverified)`. Read the device's state and observe the controlled host before deciding whether to issue the operation again.
    #[error("control outcome is uncertain; do not replay automatically: {0}")]
    AmbiguousControl(String),
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

/// Device-reported mouse movement mode; unknown means the firmware did not expose it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseMode {
    Absolute,
    Relative,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Capabilities {
    pub status: Capability,
    pub snapshot: Capability,
    pub keyboard: Capability,
    pub mouse: Capability,
    pub mouse_mode: MouseMode,
    pub live_video: Capability,
    pub power: Capability,
    pub switch: Capability,
    pub gpio: Capability,
    pub media: Capability,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum PowerAction {
    On,
    Off,
    OffHard,
    ResetHard,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Control {
    Power(PowerAction),
    Switch { port: u32 },
    Gpio { channel: String, state: bool },
    MediaConnect { connected: bool },
    MediaSelect { image: String },
}

/// Explicit consent for one consequential device operation.
pub struct Confirmation(());
impl Confirmation {
    pub fn new() -> Self {
        Self(())
    }
}
impl Default for Confirmation {
    fn default() -> Self {
        Self::new()
    }
}

/// Observation after a control request. Acknowledgement alone is not verification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlOutcome {
    ObservedMatching,
    Unverified,
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
    /// Releases tracked inputs, attempting each release within the device transport deadline. Returns an error if any release cannot be confirmed.
    async fn release_inputs(&self) -> Result<()>;
    async fn control(
        &self,
        operation: Control,
        confirmation: Confirmation,
    ) -> Result<ControlOutcome>;
}
