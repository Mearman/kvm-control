use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
use serde_json::Value;

fn invoke(command: &[&str], responses: Vec<&'static [u8]>) -> (std::process::Output, Vec<String>) {
    invoke_with_signal(command, responses, false)
}

fn invoke_with_signal(
    command: &[&str],
    responses: Vec<&'static [u8]>,
    signal_press: bool,
) -> (std::process::Output, Vec<String>) {
    let (sender, receiver) = std::sync::mpsc::channel::<u32>();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
        )
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let deadline = Instant::now() + Duration::from_secs(10);
            let socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            && Instant::now() < deadline =>
                    {
                        thread::sleep(Duration::from_millis(10))
                    }
                    Err(e) => panic!("fake device accept failed: {e}"),
                }
            };
            socket.set_nonblocking(false).unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            socket
                .set_write_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut stream = StreamOwned::new(
                ServerConnection::new(Arc::new(config.clone())).unwrap(),
                socket,
            );
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
                assert_ne!(count, 0, "fake device received incomplete request");
                bytes.extend_from_slice(&buffer[..count]);
                if let Some(pos) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
                    let header = String::from_utf8_lossy(&bytes[..pos]);
                    let length = header
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if bytes.len() >= pos + 4 + length {
                        break;
                    }
                }
            }
            let request = String::from_utf8(bytes).unwrap();
            if signal_press
                && request.starts_with("POST /api/hid/events/send_key?key=KeyA&state=true ")
            {
                let pid = receiver.recv_timeout(Duration::from_secs(10)).unwrap();
                assert!(
                    Command::new("kill")
                        .args(["-TERM", &pid.to_string()])
                        .status()
                        .unwrap()
                        .success()
                );
                requests.push(request);
                continue;
            }
            requests.push(request);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                response.len()
            )
            .unwrap();
            stream.write_all(response).unwrap();
            stream.flush().unwrap();
        }
        requests
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_kvm-cli"))
        .args([
            "--endpoint",
            &format!("https://localhost:{}", address.port()),
            "--profile",
            "pikvm",
            "--username",
            "synthetic",
            "--password-stdin",
            "--insecure-tls",
        ])
        .args(command)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    sender.send(child.id()).unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"synthetic-password\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    (output, server.join().unwrap())
}

#[test]
fn status_is_json_and_authentication_is_not_leaked() {
    let body = b"{\"ok\":true,\"result\":{\"online\":true}}";
    let (output, requests) = invoke(
        &["status"],
        vec![b"{\"ok\":true,\"result\":{}}", body, body, body, body],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["result"]["status"]["hid"]["online"], true);
    assert!(requests[0].starts_with("POST /api/auth/login "));
    assert!(requests[1].starts_with("GET /api/auth/check "));
    assert!(requests[2].starts_with("GET /api/info "));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-password"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-password"));
}

#[test]
fn capabilities_include_mouse_mode() {
    let ok = b"{\"ok\":true,\"result\":{}}";
    let hid = b"{\"ok\":true,\"result\":{\"mouse\":{\"absolute\":false}}}";
    let (output, requests) = invoke(&["capabilities"], vec![ok, ok, ok, hid, ok, ok, ok, ok, ok]);
    assert!(requests[2].starts_with("GET /api/info "));
    assert!(requests[3].starts_with("GET /api/hid "));
    assert!(requests[4].starts_with("GET /api/streamer "));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["result"]["capabilities"]["mouse_mode"], "Relative");
}

#[test]
fn absolute_move_rejects_relative_and_unknown_modes() {
    let ok = b"{\"ok\":true,\"result\":{}}";
    for hid in [
        b"{\"ok\":true,\"result\":{\"mouse\":{\"absolute\":false}}}".as_slice(),
        ok.as_slice(),
    ] {
        let (output, requests) = invoke(&["move", "1", "2"], vec![ok, ok, hid]);
        assert!(!output.status.success());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
        assert_eq!(requests.len(), 3);
        assert!(requests[2].starts_with("GET /api/hid "));
    }
}

#[test]
fn approval_pending_is_reported_and_polled_before_status() {
    let login = b"{\"ok\":true,\"result\":{\"two_step_required\":true,\"two_step_token\":\"synthetic-approval\",\"expires_in\":60}}";
    let pending = b"{\"ok\":true,\"result\":{\"status\":\"pending\"}}";
    let approved = b"{\"ok\":true,\"result\":{\"token\":\"synthetic-session\"}}";
    let body = b"{\"ok\":true,\"result\":{\"online\":true}}";
    let (output, requests) = invoke(
        &["status"],
        vec![login, pending, approved, body, body, body, body],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8(output.stderr).unwrap();
    assert!(stderr.contains("approval_pending"));
    assert!(!stderr.contains("synthetic-approval"));
    assert!(requests[1].starts_with("POST /api/auth/two_step_complete "));
    assert!(requests[2].starts_with("POST /api/auth/two_step_complete "));
    assert!(requests[3].starts_with("GET /api/auth/check "));
    assert!(requests[4].starts_with("GET /api/info "));
}

#[test]
fn expired_approval_does_not_submit_a_poll() {
    let login = b"{\"ok\":true,\"result\":{\"two_step_required\":true,\"two_step_token\":\"synthetic-approval\",\"expires_in\":0}}";
    let (output, requests) = invoke(&["status"], vec![login]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("device approval expired"));
    assert_eq!(requests.len(), 1);
}

#[cfg(unix)]
#[test]
fn sigterm_during_key_press_releases_input() {
    let ok = b"{\"ok\":true,\"result\":{}}";
    let (output, requests) = invoke_with_signal(&["key", "KeyA"], vec![ok, ok, ok, ok], true);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("interrupted"));
    assert!(requests[2].starts_with("POST /api/hid/events/send_key?key=KeyA&state=true "));
    assert!(requests[3].starts_with("POST /api/hid/events/send_key?key=KeyA&state=false "));
}

#[test]
fn key_press_is_followed_by_release() {
    let (output, requests) = invoke(
        &["key", "KeyA"],
        vec![
            b"{\"ok\":true,\"result\":{}}",
            b"{\"ok\":true,\"result\":{}}",
            b"{\"ok\":true,\"result\":{}}",
            b"{\"ok\":true,\"result\":{}}",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"]["released"],
        true
    );
    assert!(requests[1].starts_with("GET /api/auth/check "));
    assert!(requests[2].starts_with("POST /api/hid/events/send_key?key=KeyA&state=true "));
    assert!(requests[3].starts_with("POST /api/hid/events/send_key?key=KeyA&state=false "));
}
