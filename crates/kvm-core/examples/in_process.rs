//! Run with `cargo run -p kvm-core --example in_process`.
//! This consumer uses a synthetic device in process; it needs no hardware or credentials.

use async_trait::async_trait;
use kvm_core::{
    Capabilities, Capability, Confirmation, Control, ControlOutcome, Device, DeviceStatus, Error,
    MouseMode, Result, Snapshot, VideoStream,
};
use serde_json::json;
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
        Ok(DeviceStatus {
            info: json!({"device": "synthetic"}),
            hid: json!({}),
            streamer: json!({}),
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
    if capabilities.status == Capability::Supported {
        println!("Status: {}", device.status().await?.info);
    }
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
        .control(Control::Switch { port: 1 }, Confirmation::new())
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
