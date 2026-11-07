//! Typed device status.
//!
//! Each subsystem and each observed state value is wrapped in [`Reported`], so a caller can tell a value the device reported from a subsystem or field it does not have and from one whose state was not observed. Identifiers that locate an entry (a switch port number, a GPIO channel, an image or partition name), the parts of a composite value (output lists, ranges, resolutions) and the mouse mode, which has its own `Unknown`, are not wrapped. A whole subsystem is `Unsupported` when the device reports it absent or disabled, and `Unknown` when it could not be read: the session may not read it, the route failed (an HTTP error, a rejected or malformed response, an oversized body) or its response has an unexpected shape. A failed route does not discard the others. A route that did not answer in time after connecting, or whose body broke off, is unknown in the same way. A status read fails as a whole only when its evidence does not describe a device: no connection or TLS session could be established within the connect phase of the adapter's timeout (even after earlier routes were read), the endpoint is not a KVMD API (its info, HID or streamer route does not exist), or no route could be read at all. The adapter documents the exact rules.
//!
//! A subsystem whose route was denied is `Unknown`, which keeps what a partly permitted session may read. Invalid or expired credentials can produce the same picture when some route is still readable without them, so `Unknown` from a denied route may mean the credentials are invalid or expired; confirm with `check_auth` or a new login.
//!
//! Status carries no credentials. Values the device reports that can embed them, such as a remote media image URL, are not part of this model.

use serde::{Deserialize, Serialize};

use crate::{Capability, MouseMode};

/// A status value as observed from the device.
///
/// Serialised with a `support` tag (`Supported`, `Unsupported` or `Unknown`) and, for `Supported`, the value under `value`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "support", content = "value")]
pub enum Reported<T> {
    /// The device reported this value.
    Supported(T),
    /// The device reports the subsystem absent or disabled, or omits a field that the selected profile's inspected source never reports.
    Unsupported,
    /// The value was not observed; it is not evidence of support or of its absence.
    Unknown,
}

impl<T> Reported<T> {
    /// The capability this observation establishes, without its value.
    pub fn capability(&self) -> Capability {
        match self {
            Self::Supported(_) => Capability::Supported,
            Self::Unsupported => Capability::Unsupported,
            Self::Unknown => Capability::Unknown,
        }
    }

    /// The reported value, or `None` when the value is unsupported or unknown.
    pub fn value(&self) -> Option<&T> {
        match self {
            Self::Supported(value) => Some(value),
            Self::Unsupported | Self::Unknown => None,
        }
    }
}

/// Read-only device state, collected without sending any control or input request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceStatus {
    pub info: Reported<SystemInfo>,
    pub hid: Reported<HidStatus>,
    pub streamer: Reported<StreamerStatus>,
    pub atx: Reported<AtxStatus>,
    pub switch: Reported<SwitchStatus>,
    pub gpio: Reported<GpioStatus>,
    pub media: Reported<MediaStatus>,
}

/// Software and platform identification reported by the device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SystemInfo {
    /// Version of the device's KVM daemon.
    pub kvmd_version: Reported<String>,
    /// Executable name of the video streamer.
    pub streamer_app: Reported<String>,
    /// Version the device reports for its video streamer.
    pub streamer_version: Reported<String>,
    pub kernel: Reported<KernelInfo>,
    pub platform: Reported<PlatformInfo>,
    /// Host name the device advertises for itself.
    pub host: Reported<String>,
}

/// Operating-system kernel identification, as reported by `uname`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KernelInfo {
    pub system: Reported<String>,
    pub release: Reported<String>,
    pub version: Reported<String>,
    pub machine: Reported<String>,
}

/// Hardware platform identification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlatformInfo {
    /// Platform family.
    pub kind: Reported<String>,
    /// Board description from the device tree.
    pub base: Reported<String>,
    pub serial: Reported<String>,
    pub model: Reported<String>,
    pub video: Reported<String>,
    pub board: Reported<String>,
}

/// Keyboard and mouse emulation state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HidStatus {
    pub online: Reported<bool>,
    pub busy: Reported<bool>,
    /// Whether the emulated devices are connected to the attached host.
    pub connected: Reported<bool>,
    pub keyboard: Reported<KeyboardStatus>,
    pub mouse: Reported<MouseStatus>,
    pub jiggler: Reported<JigglerStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardStatus {
    pub online: Reported<bool>,
    pub leds: Reported<KeyboardLeds>,
    pub outputs: Reported<HidOutputs>,
}

/// Lock-key indicators the attached host has set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardLeds {
    pub caps: Reported<bool>,
    pub scroll: Reported<bool>,
    pub num: Reported<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseStatus {
    pub online: Reported<bool>,
    pub mode: MouseMode,
    pub outputs: Reported<HidOutputs>,
}

/// Emulated device variants the device can present, and the one presented.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HidOutputs {
    pub available: Vec<String>,
    /// `None` when the device reports no active output.
    pub active: Option<String>,
}

/// Mouse jiggler, which moves the pointer periodically to keep the host awake.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JigglerStatus {
    pub enabled: Reported<bool>,
    pub active: Reported<bool>,
    pub interval_seconds: Reported<u64>,
}

/// Video streamer configuration and, while the streamer runs, its capture state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamerStatus {
    pub features: StreamerFeatures,
    pub desired_fps: Reported<u32>,
    pub desired_fps_limits: Reported<Limits>,
    /// JPEG quality percentage.
    pub quality: Reported<u32>,
    /// Configured capture resolution, such as `1920x1080`.
    pub resolution: Reported<String>,
    pub available_resolutions: Reported<Vec<String>>,
    pub h264_bitrate: Reported<u32>,
    pub h264_gop: Reported<u32>,
    /// Encoder name reported by the running streamer.
    pub encoder: Reported<String>,
    /// Capture source state, reported only while the streamer runs.
    pub source: Reported<VideoSource>,
    /// Number of clients of the running streamer's MJPEG stream.
    pub clients: Reported<u32>,
}

/// Streamer parameters the device can configure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StreamerFeatures {
    pub quality: Reported<bool>,
    pub resolution: Reported<bool>,
    pub h264: Reported<bool>,
    pub h265: Reported<bool>,
    pub zero_delay: Reported<bool>,
    pub venc_mode: Reported<bool>,
}

/// An inclusive range.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    pub min: u32,
    pub max: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoSource {
    /// Whether the capture source has a signal.
    pub online: Reported<bool>,
    /// Captured frame size, `Unknown` while the source has no signal. A streamer started with a configured fake size (uStreamer's `--fake-width` and `--fake-height`) reports that size instead, which the device's response does not distinguish from a captured one.
    pub resolution: Reported<Resolution>,
    pub captured_fps: Reported<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Resolution {
    pub width: u32,
    pub height: u32,
}

/// Attached host's power and drive activity indicators and the state of the power and reset buttons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AtxStatus {
    /// Whether a button press is in progress.
    pub busy: Reported<bool>,
    pub power_led: Reported<bool>,
    pub hdd_led: Reported<bool>,
    /// Whether a power button press is in progress.
    pub power_busy: Reported<bool>,
    /// Whether a reset button press is in progress.
    pub reset_busy: Reported<bool>,
}

/// KVM switch inventory and selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchStatus {
    pub ports: Reported<Vec<SwitchPort>>,
    /// Selected port; `None` when the device reports that no port is selected.
    pub active_port: Reported<Option<u32>>,
    /// Whether every switch unit has applied the selection and configuration.
    pub synced: Reported<bool>,
}

/// A switch port. `port` is the number a switch control takes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SwitchPort {
    pub port: u32,
    pub unit: Reported<u32>,
    pub channel: Reported<u32>,
    /// Identifier the device displays, such as `1.2`.
    pub id: Reported<String>,
    pub name: Reported<String>,
}

/// User-configured GPIO channels.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpioStatus {
    pub inputs: Reported<Vec<GpioInput>>,
    /// Outputs that can be switched or pulsed; constant outputs are not listed.
    pub outputs: Reported<Vec<GpioOutput>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpioInput {
    pub channel: String,
    pub online: Reported<bool>,
    /// `Unknown` while the channel is offline.
    pub state: Reported<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpioOutput {
    pub channel: String,
    pub online: Reported<bool>,
    pub busy: Reported<bool>,
    /// `Unknown` while the channel is offline or busy.
    pub state: Reported<bool>,
    /// Whether a GPIO switch control is accepted for this channel.
    pub switchable: Reported<bool>,
}

/// Virtual media (mass-storage drive emulation) state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaStatus {
    pub online: Reported<bool>,
    pub busy: Reported<bool>,
    pub drive: Reported<MediaDrive>,
    /// Second emulated drive backed by a storage partition.
    pub partition_drive: Reported<MediaDrive>,
    /// Images stored on the device, any of which a media selection can name.
    pub images: Reported<Vec<MediaImage>>,
    pub partitions: Reported<Vec<StoragePartition>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDrive {
    pub connected: Reported<bool>,
    pub cdrom: Reported<bool>,
    /// Whether the drive is presented as writable.
    pub rw: Reported<bool>,
    /// Selected image; `None` when the device reports that no image is selected.
    pub image: Reported<Option<MediaImage>>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaImage {
    pub name: String,
    /// Size in bytes.
    pub size: Reported<u64>,
    /// Whether the image finished uploading.
    pub complete: Reported<bool>,
    pub removable: Reported<bool>,
}

/// A storage partition holding media images.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoragePartition {
    /// Partition name; the device reports the root partition with an empty name.
    pub name: String,
    /// Size in bytes.
    pub size: Reported<u64>,
    /// Free space in bytes.
    pub free: Reported<u64>,
    pub writable: Reported<bool>,
}
