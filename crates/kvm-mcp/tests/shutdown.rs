#![cfg(unix)]

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::post,
};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    fs,
    io::{BufRead, BufReader, Write},
    os::fd::AsRawFd,
    process::{Command, Stdio},
    sync::{Arc, Mutex},
    time::Duration,
};

async fn login() -> Json<Value> {
    Json(json!({"ok":true,"result":{}}))
}
async fn key(
    State(events): State<Arc<Mutex<Vec<bool>>>>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    events.lock().unwrap().push(query["state"] == "true");
    Json(json!({"ok":true,"result":{}}))
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_releases_pressed_key() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let pem = cert.cert.pem();
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        pem.as_bytes().to_vec(),
        cert.key_pair.serialize_pem().into_bytes(),
    )
    .await
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let app = Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/hid/events/send_key", post(key))
        .with_state(events.clone());
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, tls)
            .unwrap()
            .serve(app.into_make_service())
            .await
            .unwrap();
    });
    let temp = tempfile::tempdir().unwrap();
    let ca = temp.path().join("ca.pem");
    fs::write(&ca, pem).unwrap();
    let (reader, writer) = rustix::pipe::pipe().unwrap();
    rustix::io::fcntl_setfd(&reader, rustix::io::FdFlags::empty()).unwrap();
    let mut writer = fs::File::from(writer);
    writer.write_all(b"synthetic-password\n").unwrap();
    drop(writer);
    let mut child = Command::new(env!("CARGO_BIN_EXE_kvm-mcp"))
        .env("KVM_ENDPOINT", format!("https://localhost:{port}"))
        .env("KVM_PROFILE", "pikvm")
        .env("KVM_USERNAME", "synthetic")
        .env("KVM_PASSWORD_FD", reader.as_raw_fd().to_string())
        .env("KVM_CA_FILE", ca)
        .env("KVM_ALLOW_CONTROL", "1")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(reader);
    let mut input = child.stdin.take().unwrap();
    let mut output = BufReader::new(child.stdout.take().unwrap());
    fn request(input: &mut impl Write, output: &mut impl BufRead, value: Value) -> Value {
        writeln!(input, "{value}").unwrap();
        input.flush().unwrap();
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        serde_json::from_str(&line).unwrap()
    }
    let init = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}}),
    );
    assert!(init["result"].is_object());
    writeln!(
        input,
        "{}",
        json!({"jsonrpc":"2.0","method":"notifications/initialized"})
    )
    .unwrap();
    let response = request(
        &mut input,
        &mut output,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"key","arguments":{"key":"KeyA","pressed":true}}}),
    );
    assert_ne!(response["result"]["isError"], true);
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || child.wait().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(status.success());
    assert_eq!(*events.lock().unwrap(), vec![true, false]);
    server.abort();
}
