//! Device identity.
//!
//! A [`DeviceIdentity`] separates what the caller configured from what the device claims about itself. The endpoint label, the address and the profile are the caller's; whether the address was verified comes from the transport; the serial number and host name are the device's own reports, which nothing verifies. Nothing in it is inferred from model names or other descriptive fields. A [`DeviceId`] is derived from the configured part alone.

use serde::{Deserialize, Serialize};
use url::Url;

use crate::{DeviceProfile, Endpoint, Reported, SystemInfo};

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
    /// The serial number the device reports for its platform. It is the device's claim and is not verified: a firmware may report a placeholder, or the same value on many units, and a read that fails leaves it `Unknown`. It is therefore not part of [`DeviceId`].
    pub serial: Reported<String>,
    /// The host name the device advertises for itself; the device's claim, and one its owner can change.
    pub host: Reported<String>,
}

impl DeviceIdentity {
    /// Combines the endpoint's configuration, the transport's verification of the address, and the identifiers in the system information the device reported ([`crate::DeviceStatus::info`], or a read of that alone).
    pub fn observe(
        endpoint: &Endpoint,
        address_verified: bool,
        info: &Reported<SystemInfo>,
    ) -> Self {
        let (serial, host) = match info {
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

    /// The identifier of the device this identity describes, derived from the configuration alone: see [`DeviceId`].
    pub fn id(&self) -> DeviceId {
        DeviceId::named(self.label.as_deref(), &self.address, self.profile)
    }
}

impl Endpoint {
    /// The identifier of the device this endpoint configures: see [`DeviceId`].
    pub fn device_id(&self) -> DeviceId {
        DeviceId::named(self.label.as_deref(), &self.address, self.profile)
    }
}

/// How an endpoint is named in a [`DeviceId`]: by its label when it has one, otherwise by its address. The variants never compare equal, so a label spelt like an address does not collide with that address.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum EndpointName {
    Label(String),
    Address(Url),
}

/// A device identifier for keying stored state or recognising a configured device again, comparable and hashable.
///
/// It is stable under what the caller configured: the endpoint's label (or its address when it has none) and the selected profile, and nothing else. Nothing the device reports is part of it, so a failed read, a firmware that starts or stops reporting a serial number, or a renamed host leaves it unchanged; a label lets a device keep its identifier when its address changes. It identifies the configuration, not the hardware: two endpoints reaching the same device have different identifiers, and replacing the device behind an endpoint keeps the identifier. Compare [`DeviceIdentity::serial`] separately where the device's own claim matters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct DeviceId {
    pub endpoint: EndpointName,
    pub profile: DeviceProfile,
}

impl DeviceId {
    fn named(label: Option<&str>, address: &Url, profile: DeviceProfile) -> Self {
        Self {
            endpoint: match label {
                Some(label) => EndpointName::Label(label.to_owned()),
                None => EndpointName::Address(address.clone()),
            },
            profile,
        }
    }
}
