use async_trait::async_trait;
use kvm_core::{
    AtxStatus, Capabilities, Capability, Confirmation, Control, ControlOutcome, Device,
    DeviceStatus, MouseMode, Reported, Result, Snapshot,
};
use kvm_mcp::Server;
use rmcp::ServiceExt;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Fake(AtomicUsize);
#[async_trait]
impl Device for Fake {
    async fn video(&self) -> Result<Box<dyn kvm_core::VideoStream>> {
        Err(kvm_core::Error::Unsupported("video".into()))
    }
    async fn status(&self) -> Result<DeviceStatus> {
        Ok(DeviceStatus {
            info: Reported::Unknown,
            hid: Reported::Unknown,
            streamer: Reported::Unknown,
            atx: Reported::Supported(AtxStatus {
                busy: Reported::Supported(false),
                power_led: Reported::Supported(true),
                hdd_led: Reported::Unknown,
                power_busy: Reported::Unsupported,
                reset_busy: Reported::Unsupported,
            }),
            switch: Reported::Unsupported,
            gpio: Reported::Unsupported,
            media: Reported::Unsupported,
        })
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        Ok(Capabilities {
            status: Capability::Supported,
            snapshot: Capability::Unknown,
            keyboard: Capability::Unknown,
            mouse: Capability::Unknown,
            mouse_mode: MouseMode::Relative,
            live_video: Capability::Unsupported,
            power: Capability::Unknown,
            switch: Capability::Unknown,
            gpio: Capability::Unknown,
            media: Capability::Unknown,
        })
    }
    async fn snapshot(&self) -> Result<Snapshot> {
        Ok(Snapshot {
            jpeg: vec![255, 216, 255, 217],
        })
    }
    async fn key(&self, _: &str, _: bool) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
    async fn mouse_button(&self, _: &str, _: bool) -> Result<()> {
        Ok(())
    }
    async fn mouse_move(&self, _: i16, _: i16) -> Result<()> {
        Ok(())
    }
    async fn release_inputs(&self) -> Result<()> {
        Ok(())
    }
    async fn control(&self, operation: Control, _: Confirmation) -> Result<ControlOutcome> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(match operation {
            Control::Switch { .. } => ControlOutcome::ObservedMatching,
            _ => ControlOutcome::Unverified,
        })
    }
}

#[tokio::test]
async fn protocol_round_trip() {
    let fake = Arc::new(Fake(AtomicUsize::new(0)));
    let (client, server) = tokio::io::duplex(16384);
    let task = tokio::spawn(async move {
        Server::new(fake.clone(), false, false)
            .serve(server)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
        fake
    });
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();
    async fn call(
        write: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>,
        lines: &mut tokio::io::Lines<BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
        message: Value,
    ) -> Value {
        write
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap()
    }
    let init = call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await;
    assert!(init["result"]["capabilities"]["tools"].is_object());
    write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    let list = call(
        &mut write,
        &mut lines,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let tools = list["result"]["tools"].as_array().unwrap();
    assert!(tools.iter().any(|tool| tool["name"] == "key"
        && tool["inputSchema"]["properties"]["pressed"]["type"] == "boolean"));
    let caps = call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":5,"method":"tools/call","params":{"name":"capabilities","arguments":{}}})).await;
    let capabilities: Value =
        serde_json::from_str(caps["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(capabilities["snapshot"], "Unknown");
    assert_eq!(capabilities["mouse_mode"], "Relative");
    assert_eq!(capabilities["live_video"], "Unsupported");
    let status = call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"status","arguments":{}}})).await;
    let status: Value =
        serde_json::from_str(status["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(
        status["atx"]["value"]["power_led"],
        json!({"support": "Supported", "value": true})
    );
    assert_eq!(
        status["atx"]["value"]["power_busy"],
        json!({"support": "Unsupported"})
    );
    assert_eq!(status["switch"], json!({"support": "Unsupported"}));
    assert_eq!(status["info"], json!({"support": "Unknown"}));
    let image = call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"snapshot","arguments":{}}})).await;
    assert_eq!(image["result"]["content"][0]["type"], "image");
    assert_eq!(image["result"]["content"][0]["mimeType"], "image/jpeg");
    assert_eq!(image["result"]["content"][0]["data"], "/9j/2Q==");
    let denied = call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"key","arguments":{"key":"KeyA","pressed":true}}})).await;
    assert_eq!(denied["result"]["isError"], true);
    let denied_power = call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":6,"method":"tools/call","params":{"name":"consequential","arguments":{"operation":{"kind":"power","action":"off_hard","confirmed":true}}}})).await;
    assert_eq!(denied_power["result"]["isError"], true);
    drop(write);
    drop(lines);
    let fake = task.await.unwrap();
    assert_eq!(fake.0.load(Ordering::SeqCst), 0);

    let fake = Arc::new(Fake(AtomicUsize::new(0)));
    let (client, server) = tokio::io::duplex(16384);
    let server_fake = fake.clone();
    let task = tokio::spawn(async move {
        Server::new(server_fake, false, true)
            .serve(server)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap()
    });
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();
    call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).await;
    write
        .write_all(b"{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}\n")
        .await
        .unwrap();
    let no_confirmation = call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"consequential","arguments":{"operation":{"kind":"switch","port":1,"confirmed":false}}}})).await;
    assert_eq!(no_confirmation["result"]["isError"], true);
    assert_eq!(fake.0.load(Ordering::SeqCst), 0);
    let observed = call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"consequential","arguments":{"operation":{"kind":"switch","port":1,"confirmed":true}}}})).await;
    assert!(
        observed["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("ObservedMatching")
    );
    let unverified = call(&mut write, &mut lines, json!({"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"consequential","arguments":{"operation":{"kind":"power","action":"on","confirmed":true}}}})).await;
    assert!(
        unverified["result"]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Unverified")
    );
    assert_eq!(fake.0.load(Ordering::SeqCst), 2);
    drop(write);
    drop(lines);
    task.await.unwrap();
}
