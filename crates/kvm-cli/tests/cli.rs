use std::{
    io::{Read, Write},
    net::TcpListener,
    process::{Command, Stdio},
    sync::Arc,
    thread,
};

use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
use serde_json::Value;

fn invoke(command: &[&str], responses: Vec<&'static [u8]>) -> (std::process::Output, Vec<String>) {
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
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let (socket, _) = listener.accept().unwrap();
            let mut stream = StreamOwned::new(
                ServerConnection::new(Arc::new(config.clone())).unwrap(),
                socket,
            );
            let mut bytes = Vec::new();
            let mut buffer = [0; 1024];
            loop {
                let count = stream.read(&mut buffer).unwrap();
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
            requests.push(String::from_utf8(bytes).unwrap());
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
    let (output, requests) = invoke(&["status"], vec![b"", body, body, body]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let result: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(result["result"]["status"]["hid"]["online"], true);
    assert!(requests[0].starts_with("POST /api/auth/login "));
    assert!(requests[1].starts_with("GET /api/info "));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("synthetic-password"));
    assert!(!String::from_utf8_lossy(&output.stderr).contains("synthetic-password"));
}

#[test]
fn key_press_is_followed_by_release() {
    let (output, requests) = invoke(&["key", "KeyA"], vec![b"", b"", b""]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&output.stdout).unwrap()["result"]["released"],
        true
    );
    assert!(requests[1].starts_with("POST /api/hid/events/send_key?key=KeyA&state=true "));
    assert!(requests[2].starts_with("POST /api/hid/events/send_key?key=KeyA&state=false "));
}
