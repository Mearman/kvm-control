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
            switch: Reported::Supported(SwitchStatus {
                ports: Reported::Supported(vec![port(0), port(1)]),
                active_port: Reported::Supported(Some(0)),
                synced: Reported::Supported(true),
            }),
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

// A consumer depends on the public contract, not on a particular device implementation.
async fn consume(device: &dyn Device) -> Result<ControlOutcome> {
    let capabilities = device.capabilities().await?;
    if capabilities.status != Capability::Supported {
        return Err(Error::Unsupported("status".into()));
    }
    // Status reports which switch ports exist, so the consumer picks one the device has rather than guessing.
    let status = device.status().await?;
    let switch = status
        .switch
        .value()
        .ok_or_else(|| Error::Unsupported("switch status".into()))?;
    let active = switch.active_port.value().copied().flatten();
    let target = switch
        .ports
        .value()
        .and_then(|ports| ports.iter().find(|port| Some(port.port) != active))
        .ok_or_else(|| Error::Unsupported("no other switch port".into()))?
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
    device
        .control(Control::Switch { port: target }, Confirmation::new())
        .await
}

#[tokio::main]
async fn main() -> Result<()> {
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
}
