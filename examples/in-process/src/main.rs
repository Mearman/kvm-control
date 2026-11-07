//! Run with `cargo run --manifest-path examples/in-process/Cargo.toml` from the repository root.
//! Test with `cargo test --manifest-path examples/in-process/Cargo.toml`.
//! This consumer uses a synthetic device in process; it needs no hardware or credentials.

use async_trait::async_trait;
use kvm_core::{
    Capabilities, Capability, Confirmation, Control, ControlOutcome, Device, DeviceStatus, Error,
    MouseMode, Reported, Result, Snapshot, SwitchPort, SwitchStatus, VideoStream,
};
use std::sync::Mutex;

#[derive(Default)]
struct FakeDevice {
    events: Mutex<Vec<String>>,
    /// Reports the switch status unknown, as a device whose switch route failed does.
    switch_unknown: bool,
    /// Reports the switch's active port unknown, as a device whose switch summary is unreadable does.
    active_unknown: bool,
    /// Reports the switch's port list unsupported.
    no_port_list: bool,
}

struct FakeVideo(bool);

#[async_trait]
impl VideoStream for FakeVideo {
    async fn next_frame(&mut self) -> Result<Option<Snapshot>> {
        if self.0 {
            self.0 = false;
            Ok(Some(Snapshot {
                jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
            }))
        } else {
            Ok(None)
        }
    }
}

#[async_trait]
impl Device for FakeDevice {
    async fn video(&self) -> Result<Box<dyn VideoStream>> {
        Ok(Box::new(FakeVideo(true)))
    }
    async fn status(&self) -> Result<DeviceStatus> {
        let port = |port| SwitchPort {
            port,
            unit: Reported::Supported(0),
            channel: Reported::Supported(port),
            id: Reported::Supported((port + 1).to_string()),
            name: Reported::Unknown,
        };
        Ok(DeviceStatus {
            info: Reported::Unknown,
            hid: Reported::Unknown,
            streamer: Reported::Unknown,
            atx: Reported::Unsupported,
            switch: if self.switch_unknown {
                Reported::Unknown
            } else {
                Reported::Supported(SwitchStatus {
                    ports: if self.no_port_list {
                        Reported::Unsupported
                    } else {
                        Reported::Supported(vec![port(0), port(1)])
                    },
                    active_port: if self.active_unknown {
                        Reported::Unknown
                    } else {
                        Reported::Supported(Some(0))
                    },
                    synced: Reported::Supported(true),
                })
            },
            gpio: Reported::Unsupported,
            media: Reported::Unsupported,
        })
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        Ok(Capabilities {
            status: Capability::Supported,
            snapshot: Capability::Supported,
            keyboard: Capability::Supported,
            mouse: Capability::Unsupported,
            mouse_mode: MouseMode::Unknown,
            live_video: Capability::Supported,
            power: Capability::Unsupported,
            switch: Capability::Supported,
            gpio: Capability::Unsupported,
            media: Capability::Unsupported,
        })
    }
    async fn snapshot(&self) -> Result<Snapshot> {
        Ok(Snapshot {
            jpeg: vec![0xff, 0xd8, 0xff, 0xd9],
        })
    }
    async fn key(&self, key: &str, pressed: bool) -> Result<()> {
        self.events
            .lock()
            .unwrap()
            .push(format!("key {key} {pressed}"));
        Ok(())
    }
    async fn mouse_button(&self, _: &str, _: bool) -> Result<()> {
        Err(Error::Unsupported("mouse".into()))
    }
    async fn mouse_move(&self, _: i16, _: i16) -> Result<()> {
        Err(Error::Unsupported("mouse".into()))
    }
    async fn release_inputs(&self) -> Result<()> {
        self.events.lock().unwrap().push("release inputs".into());
        Ok(())
    }
    async fn control(&self, operation: Control, _: Confirmation) -> Result<ControlOutcome> {
        match operation {
            Control::Switch { port } => {
                self.events.lock().unwrap().push(format!("switch {port}"));
                Ok(ControlOutcome::Unverified)
            }
            _ => Err(Error::Unsupported("control".into())),
        }
    }
}

/// Why the consumer could not choose a switch port, beside the device's own errors.
#[derive(Debug)]
enum ConsumeError {
    Device(Error),
    /// The device reports that it has no switch.
    NoSwitch,
    /// The switch status, its port list or its active port could not be observed, so no port can be chosen safely.
    SwitchUnknown,
    /// The device reports a switch but no port list.
    NoPortList,
    NoOtherPort,
}

impl From<Error> for ConsumeError {
    fn from(error: Error) -> Self {
        Self::Device(error)
    }
}

impl std::fmt::Display for ConsumeError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Device(error) => error.fmt(formatter),
            Self::NoSwitch => formatter.write_str("the device has no switch"),
            Self::SwitchUnknown => formatter.write_str("the switch status is unknown"),
            Self::NoPortList => formatter.write_str("the switch reports no port list"),
            Self::NoOtherPort => formatter.write_str("the switch has no other port"),
        }
    }
}

impl std::error::Error for ConsumeError {}

// A consumer depends on the public contract, not on a particular device implementation.
async fn consume(device: &dyn Device) -> std::result::Result<ControlOutcome, ConsumeError> {
    let capabilities = device.capabilities().await?;
    // Status is read whatever its capability says: each subsystem reports its own support, and the switch subsystem reports which ports exist, so the consumer picks one the device has rather than guessing.
    let status = device.status().await?;
    let switch = match &status.switch {
        Reported::Supported(switch) => switch,
        Reported::Unsupported => return Err(ConsumeError::NoSwitch),
        Reported::Unknown => return Err(ConsumeError::SwitchUnknown),
    };
    // An unobserved active port is not "no port selected": choosing a target then could pick the active port.
    let active = match switch.active_port {
        Reported::Supported(active) => active,
        Reported::Unsupported | Reported::Unknown => return Err(ConsumeError::SwitchUnknown),
    };
    let ports = match &switch.ports {
        Reported::Supported(ports) => ports,
        Reported::Unsupported => return Err(ConsumeError::NoPortList),
        Reported::Unknown => return Err(ConsumeError::SwitchUnknown),
    };
    let target = ports
        .iter()
        .find(|port| Some(port.port) != active)
        .ok_or(ConsumeError::NoOtherPort)?
        .port;
    println!("Switch ports: {:?}, active {active:?}", switch.ports);
    if capabilities.snapshot == Capability::Supported {
        println!("Snapshot: {} bytes", device.snapshot().await?.jpeg.len());
    }
    if capabilities.live_video == Capability::Supported {
        let mut video = device.video().await?;
        if let Some(frame) = video.next_frame().await? {
            println!("Video frame: {} bytes", frame.jpeg.len());
        }
    }
    if capabilities.keyboard == Capability::Supported {
        let result = device.key("KeyA", true).await;
        // Release even when the press has an ambiguous outcome.
        let release = device.release_inputs().await;
        result?;
        release?;
    }
    Ok(device
        .control(Control::Switch { port: target }, Confirmation::new())
        .await?)
}

#[tokio::main]
async fn main() -> std::result::Result<(), ConsumeError> {
    let device = FakeDevice::default();
    let outcome = consume(&device).await?;
    println!("Switch outcome: {outcome:?}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn consumer_uses_the_device_contract_in_process() {
        let device = FakeDevice::default();
        assert_eq!(consume(&device).await.unwrap(), ControlOutcome::Unverified);
        assert_eq!(
            *device.events.lock().unwrap(),
            ["key KeyA true", "release inputs", "switch 1"]
        );
        let mut video = device.video().await.unwrap();
        assert_eq!(
            video.next_frame().await.unwrap().unwrap().jpeg,
            [0xff, 0xd8, 0xff, 0xd9]
        );
        assert!(video.next_frame().await.unwrap().is_none());
        assert!(matches!(
            device.mouse_move(1, 1).await,
            Err(Error::Unsupported(_))
        ));
    }

    #[tokio::test]
    async fn an_unobserved_switch_sends_no_control() {
        for (device, expected) in [
            (
                FakeDevice {
                    switch_unknown: true,
                    ..FakeDevice::default()
                },
                "the switch status is unknown",
            ),
            (
                FakeDevice {
                    active_unknown: true,
                    ..FakeDevice::default()
                },
                "the switch status is unknown",
            ),
            (
                FakeDevice {
                    no_port_list: true,
                    ..FakeDevice::default()
                },
                "the switch reports no port list",
            ),
        ] {
            let error = consume(&device).await.unwrap_err();
            assert_eq!(error.to_string(), expected);
            // No switch control was sent without a known target.
            assert!(
                !device
                    .events
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|event| event.starts_with("switch"))
            );
        }
    }
}
