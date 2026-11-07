#![cfg(unix)]

use axum::{
    Json, Router,
    extract::{Query, State},
    routing::{get, post},
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
use tokio::sync::Notify;

async fn login() -> Json<Value> {
    Json(json!({"ok":true,"result":{}}))
}
#[derive(Default)]
struct Fixture {
    events: Mutex<Vec<bool>>,
    entered: Notify,
    proceed: Notify,
    approved: Notify,
    approval_called: Notify,
}

async fn approval_login() -> Json<Value> {
    Json(
        json!({"ok":true,"result":{"two_step_required":true,"two_step_token":"synthetic-token","expires_in":30}}),
    )
}

async fn approval_complete(State(fixture): State<Arc<Fixture>>) -> Json<Value> {
    fixture.approval_called.notify_one();
    fixture.approved.notified().await;
    Json(json!({"ok":true,"result":{"token":"synthetic-session"}}))
}

async fn delayed_key(
    State(fixture): State<Arc<Fixture>>,
    Query(query): Query<HashMap<String, String>>,
) -> Json<Value> {
    let pressed = query["state"] == "true";
    fixture.events.lock().unwrap().push(pressed);
    if pressed {
        fixture.entered.notify_one();
        fixture.proceed.notified().await;
    }
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
    exercise(false).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn sigterm_waits_for_inflight_key() {
    exercise(true).await;
}

async fn exercise(delayed: bool) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let pem = cert.cert.pem();
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        pem.as_bytes().to_vec(),
        cert.signing_key.serialize_pem().into_bytes(),
    )
    .await
    .unwrap();
    let events = Arc::new(Mutex::new(Vec::new()));
    let fixture = Arc::new(Fixture::default());
    let app = if delayed {
        Router::new()
            .route("/api/auth/login", post(login))
            .route("/api/auth/check", get(login))
            .route("/api/hid/events/send_key", post(delayed_key))
            .with_state(fixture.clone())
    } else {
        Router::new()
            .route("/api/auth/login", post(login))
            .route("/api/auth/check", get(login))
            .route("/api/hid/events/send_key", post(key))
            .with_state(events.clone())
    };
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
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut readiness = String::new();
    stderr.read_line(&mut readiness).unwrap();
    assert_eq!(readiness.trim_end(), "MCP shutdown handlers registered");
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
    let key_request = json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"key","arguments":{"key":"KeyA","pressed":true}}});
    if delayed {
        writeln!(input, "{key_request}").unwrap();
        input.flush().unwrap();
        tokio::time::timeout(Duration::from_secs(10), fixture.entered.notified())
            .await
            .unwrap();
    } else {
        let response = request(&mut input, &mut output, key_request);
        assert_ne!(response["result"]["isError"], true);
    }
    rustix::process::kill_process(
        rustix::process::Pid::from_raw(child.id() as i32).unwrap(),
        rustix::process::Signal::TERM,
    )
    .unwrap();
    if delayed {
        fixture.proceed.notify_one();
    }
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let mut remaining_stderr = String::new();
    std::io::Read::read_to_string(&mut stderr, &mut remaining_stderr).unwrap();
    let stderr = remaining_stderr
        .replace("synthetic-password", "[redacted]")
        .replace("synthetic-token", "[redacted]")
        .replace("synthetic-session", "[redacted]");
    assert!(
        output.status.success(),
        "child status: {}; stderr: {}",
        output.status,
        stderr
    );
    if delayed {
        assert_eq!(*fixture.events.lock().unwrap(), vec![true, false]);
    } else {
        assert_eq!(*events.lock().unwrap(), vec![true, false]);
    }
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

#[tokio::test(flavor = "multi_thread")]
async fn approval_precedes_mcp_initialisation() {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let pem = cert.cert.pem();
    let tls = axum_server::tls_rustls::RustlsConfig::from_pem(
        pem.as_bytes().to_vec(),
        cert.signing_key.serialize_pem().into_bytes(),
    )
    .await
    .unwrap();
    let fixture = Arc::new(Fixture::default());
    let app = Router::new()
        .route("/api/auth/login", post(approval_login))
        .route("/api/auth/check", get(login))
        .route("/api/auth/two_step_complete", post(approval_complete))
        .with_state(fixture.clone());
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
        .env("KVM_PROFILE", "glkvm")
        .env("KVM_USERNAME", "synthetic")
        .env("KVM_PASSWORD_FD", reader.as_raw_fd().to_string())
        .env("KVM_CA_FILE", ca)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    drop(reader);
    let mut input = child.stdin.take().unwrap();
    writeln!(input, "{}", json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"test","version":"1"}}})).unwrap();
    input.flush().unwrap();
    tokio::time::timeout(Duration::from_secs(10), fixture.approval_called.notified())
        .await
        .unwrap();
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = tokio::sync::oneshot::channel();
    let output_task = tokio::task::spawn_blocking(move || {
        let mut line = String::new();
        BufReader::new(stdout).read_line(&mut line).unwrap();
        let _ = tx.send(line);
    });
    let mut rx = rx;
    assert!(
        tokio::time::timeout(Duration::from_millis(200), &mut rx)
            .await
            .is_err()
    );
    fixture.approved.notify_one();
    let line = tokio::time::timeout(Duration::from_secs(10), rx)
        .await
        .unwrap()
        .unwrap();
    assert!(serde_json::from_str::<Value>(&line).unwrap()["result"].is_object());
    drop(input);
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::task::spawn_blocking(move || child.wait_with_output().unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr)
        .replace("synthetic-password", "[redacted]")
        .replace("synthetic-token", "[redacted]")
        .replace("synthetic-session", "[redacted]");
    assert!(
        output.status.success(),
        "child status: {}; stderr: {}",
        output.status,
        stderr
    );
    output_task.await.unwrap();
    server.abort();
    assert!(server.await.unwrap_err().is_cancelled());
}

/// Each required variable is named in the error, with what to set it to, instead of the bare `NotPresent` of `std::env::var`.
#[test]
fn missing_configuration_names_each_unset_variable() {
    let output = Command::new(env!("CARGO_BIN_EXE_kvm-mcp"))
        .env_clear()
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8(output.stderr).unwrap();
    for (name, expected) in [
        ("KVM_ENDPOINT", "the device HTTPS URL"),
        ("KVM_PROFILE", "pikvm or glkvm"),
        ("KVM_USERNAME", "the device account name"),
        (
            "KVM_PASSWORD_FD",
            "the number of an inherited pipe descriptor",
        ),
    ] {
        assert!(
            stderr.contains(&format!("{name} is not set (set it to {expected}")),
            "{stderr}"
        );
    }
    assert!(!stderr.contains("NotPresent"), "{stderr}");
}

/// A variable that is not valid Unicode is named, and its value, which may be sensitive, is not printed.
#[test]
fn non_unicode_configuration_is_named_without_its_value() {
    use std::{ffi::OsStr, os::unix::ffi::OsStrExt};
    let output = Command::new(env!("CARGO_BIN_EXE_kvm-mcp"))
        .env_clear()
        .env("KVM_ENDPOINT", "https://localhost")
        .env("KVM_PROFILE", "pikvm")
        .env("KVM_USERNAME", OsStr::from_bytes(b"synthetic-user-\xff"))
        .env("KVM_PASSWORD_FD", "3")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("KVM_USERNAME is not valid Unicode"),
        "{stderr}"
    );
    assert!(!stderr.contains("synthetic-user"), "{stderr}");
    assert!(!stderr.contains("is not set"), "{stderr}");
}
