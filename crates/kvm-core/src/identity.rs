//! Device identity.
//!
//! A [`DeviceIdentity`] separates what the caller configured from what the device claims about itself. The endpoint label, the address and the profile are the caller's; whether the address was verified comes from the transport; the serial number and host name are the device's own reports, which nothing verifies. Nothing in it is inferred from model names or other descriptive fields.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{DeviceProfile, DeviceStatus, Endpoint, Reported};

/// What is known about which device a connection reaches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIdentity {
    /// The endpoint's caller-chosen label, if it has one.
    pub label: Option<String>,
    /// The address the connection was opened with, as configured.
    pub address: Url,
    /// Whether the transport verified that the server answering is entitled to `address` (for HTTPS, a certificate valid for its host from a trusted authority). It says nothing about which device sits behind that server: a reverse proxy, or any device holding the certificate, passes.
    pub address_verified: bool,
    /// The profile the caller selected; never inferred from what the device reports.
    pub profile: DeviceProfile,
    /// The serial number the device reports for its platform. It is the device's claim and is not verified: a firmware may report a placeholder, or the same value on many units.
    pub serial: Reported<String>,
    /// The host name the device advertises for itself; the device's claim, and one its owner can change.
    pub host: Reported<String>,
}

impl DeviceIdentity {
    /// Combines the endpoint's configuration, the transport's verification of the address, and the identifiers the device reported in `status`.
    pub fn observe(endpoint: &Endpoint, address_verified: bool, status: &DeviceStatus) -> Self {
        let (serial, host) = match &status.info {
            Reported::Supported(info) => (
                match &info.platform {
                    Reported::Supported(platform) => platform.serial.clone(),
                    Reported::Unsupported => Reported::Unsupported,
                    Reported::Unknown => Reported::Unknown,
                },
                info.host.clone(),
            ),
            Reported::Unsupported => (Reported::Unsupported, Reported::Unsupported),
            Reported::Unknown => (Reported::Unknown, Reported::Unknown),
        };
        Self {
            label: endpoint.label.clone(),
            address: endpoint.address.clone(),
            address_verified,
            profile: endpoint.profile,
            serial,
            host,
        }
    }

    /// The stable identifier of this device: the endpoint's label (or its address when it has none), the selected profile and the reported serial number.
    ///
    /// The same endpoint, profile and serial number always give the same identifier. The endpoint part keeps two devices that report the same serial number apart, and the label lets a device keep its identifier when its address changes. An unobserved serial number stays `Unknown` in the identifier rather than being guessed, so such an identifier distinguishes only by endpoint and profile. The host name is not part of it, since it changes more readily than the hardware.
    pub fn id(&self) -> DeviceId {
        DeviceId {
            endpoint: self
                .label
                .clone()
                .unwrap_or_else(|| self.address.to_string()),
            profile: self.profile,
            serial: self.serial.clone(),
        }
    }
}

/// A stable device identifier, comparable and hashable, for keying stored state or recognising a device again. See [`DeviceIdentity::id`] for how it is derived and what it does not prove.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId {
    pub endpoint: String,
    pub profile: DeviceProfile,
    pub serial: Reported<String>,
}
