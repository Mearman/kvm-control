use async_trait::async_trait;
use kvm_core::{
    Capabilities, Capability, Confirmation, Control, Device, DeviceStatus, Result, Snapshot,
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
    async fn status(&self) -> Result<DeviceStatus> {
        Ok(DeviceStatus {
            info: json!({}),
            hid: json!({}),
            streamer: json!({}),
        })
    }
    async fn capabilities(&self) -> Result<Capabilities> {
        Ok(Capabilities {
            status: Capability::Supported,
            snapshot: Capability::Unknown,
            keyboard: Capability::Unknown,
            mouse: Capability::Unknown,
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
    async fn control(&self, _: Control, _: Confirmation) -> Result<()> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
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
    assert_eq!(capabilities["live_video"], "Unsupported");
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
}
