use std::{
    collections::HashSet,
    io::{ErrorKind, Read, Write},
    net::{TcpListener, TcpStream},
    process::{Child, Command, Stdio},
    sync::Arc,
    thread,
    time::{Duration, Instant},
};

use base64::{Engine, engine::general_purpose::STANDARD};
use rustls::{ServerConfig, ServerConnection, StreamOwned, pki_types::PrivatePkcs8KeyDer};
use serde_json::Value;

fn invoke(command: &[&str], responses: Vec<&'static [u8]>) -> (std::process::Output, Vec<String>) {
    invoke_with_signal("pikvm", command, responses, false)
}

fn tls_config() -> Arc<ServerConfig> {
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    Arc::new(
        ServerConfig::builder()
            .with_no_client_auth()
            .with_single_cert(
                vec![cert.cert.der().clone()],
                PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
            )
            .unwrap(),
    )
}

fn accept_connection(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(10);
    let socket = loop {
        match listener.accept() {
            Ok((socket, _)) => break socket,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock && Instant::now() < deadline => {
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
    socket
}

fn accept_tls(
    listener: &TcpListener,
    config: &Arc<ServerConfig>,
) -> StreamOwned<ServerConnection, TcpStream> {
    StreamOwned::new(
        ServerConnection::new(config.clone()).unwrap(),
        accept_connection(listener),
    )
}

fn read_request(stream: &mut impl Read) -> String {
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
    String::from_utf8(bytes).unwrap()
}

fn spawn_cli(port: u16, profile: &str, command: &[&str]) -> Child {
    let mut child = Command::new(env!("CARGO_BIN_EXE_kvm-cli"))
        .args([
            "--endpoint",
            &format!("https://localhost:{port}"),
            "--profile",
            profile,
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
    child
}

fn invoke_with_signal(
    profile: &str,
    command: &[&str],
    responses: Vec<&'static [u8]>,
    signal_press: bool,
) -> (std::process::Output, Vec<String>) {
    let (sender, receiver) = std::sync::mpsc::channel::<u32>();
    let config = tls_config();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();
        for response in responses {
            let mut stream = accept_tls(&listener, &config);
            let request = read_request(&mut stream);
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
    let child = spawn_cli(address.port(), profile, command);
    sender.send(child.id()).unwrap();
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
    let (output, requests) = invoke_with_signal(
        "glkvm",
        &["status"],
        vec![login, pending, approved, body, body, body, body],
        false,
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
    let (output, requests) = invoke_with_signal("glkvm", &["status"], vec![login], false);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("device approval expired"));
    assert_eq!(requests.len(), 1);
}

#[cfg(unix)]
#[test]
fn sigterm_during_key_press_releases_input() {
    let ok = b"{\"ok\":true,\"result\":{}}";
    let (output, requests) =
        invoke_with_signal("pikvm", &["key", "KeyA"], vec![ok, ok, ok, ok], true);
    // An exit status rather than death by the signal shows the handler was registered when the press was sent.
    assert_eq!(output.status.code(), Some(1), "{}", output.status);
    // The fake device signals the CLI and then closes the press connection without answering. Holding the connection open instead would stall the test: the release waits for the input lock, which the submitted press holds until its request finishes, so the release would follow only the client's request timeout. The signal is sent before the close, so in practice the runtime observes it either before the press fails, ending the press, or during the release round trip that follows; this is a scheduling argument rather than a guarantee, since tokio counts a signal as received only once its handler has written the self-pipe and a driver turn has broadcast it, but either way the interruption is reported. A press closed after submission leaves the input session indeterminate, so cleanup cannot be confirmed and the error says so.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains(r#""error":"interrupted; "#), "{stderr}");
    assert!(
        stderr.contains("; cleanup: input outcome is uncertain"),
        "{stderr}"
    );
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

/// Solid-colour baseline JPEGs that differ only in colour, each accepted unchanged by the adapter's JPEG validation.
const JPEG_FRAMES: [&str; 3] = [
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDACgcHiMeGSgjISMtKygwPGRBPDc3PHtYXUlkkYCZlo+AjIqgtObDoKrarYqMyP/L2u71////m8H////6/+b9//j/2wBDASstLTw1PHZBQXb4pYyl+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj/wAARCAAIAAgDASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAL/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFQEBAQAAAAAAAAAAAAAAAAAAAwT/xAAUEQEAAAAAAAAAAAAAAAAAAAAA/9oADAMBAAIRAxEAPwCQFAH/2Q==",
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDACgcHiMeGSgjISMtKygwPGRBPDc3PHtYXUlkkYCZlo+AjIqgtObDoKrarYqMyP/L2u71////m8H////6/+b9//j/2wBDASstLTw1PHZBQXb4pYyl+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj/wAARCAAIAAgDASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAT/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFQEBAQAAAAAAAAAAAAAAAAAAAwT/xAAUEQEAAAAAAAAAAAAAAAAAAAAA/9oADAMBAAIRAxEAPwCcAS9//9k=",
    "/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDACgcHiMeGSgjISMtKygwPGRBPDc3PHtYXUlkkYCZlo+AjIqgtObDoKrarYqMyP/L2u71////m8H////6/+b9//j/2wBDASstLTw1PHZBQXb4pYyl+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj4+Pj/wAARCAAIAAgDASIAAhEBAxEB/8QAFQABAQAAAAAAAAAAAAAAAAAAAAH/xAAUEAEAAAAAAAAAAAAAAAAAAAAA/8QAFAEBAAAAAAAAAAAAAAAAAAAABP/EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAMAwEAAhEDEQA/AKAOC//Z",
];

const WEBSOCKET_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
const SESSION_COOKIE: &str = "auth_token=synthetic-session";

/// SHA-1 (FIPS 180-4), used only to answer the WebSocket opening handshake.
fn sha1(message: &[u8]) -> [u8; 20] {
    let mut state: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    let mut padded = message.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&((message.len() as u64) * 8).to_be_bytes());
    for block in padded.as_chunks::<64>().0 {
        let mut words = [0_u32; 80];
        for (word, bytes) in words.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for index in 16..80 {
            words[index] =
                (words[index - 3] ^ words[index - 8] ^ words[index - 14] ^ words[index - 16])
                    .rotate_left(1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = state;
        for (index, word) in words.iter().enumerate() {
            let (mix, constant) = match index {
                0..=19 => ((b & c) | (!b & d), 0x5a82_7999),
                20..=39 => (b ^ c ^ d, 0x6ed9_eba1),
                40..=59 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let next = a
                .rotate_left(5)
                .wrapping_add(mix)
                .wrapping_add(e)
                .wrapping_add(constant)
                .wrapping_add(*word);
            e = d;
            d = c;
            c = b.rotate_left(30);
            b = a;
            a = next;
        }
        for (slot, value) in state.iter_mut().zip([a, b, c, d, e]) {
            *slot = slot.wrapping_add(value);
        }
    }
    let mut digest = [0; 20];
    for (bytes, word) in digest.as_chunks_mut::<4>().0.iter_mut().zip(state) {
        *bytes = word.to_be_bytes();
    }
    digest
}

#[test]
fn websocket_accept_matches_the_rfc_6455_example() {
    let accept = STANDARD.encode(sha1(
        format!("dGhlIHNhbXBsZSBub25jZQ=={WEBSOCKET_GUID}").as_bytes(),
    ));
    assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
}

fn header_value<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then_some(value.trim())
    })
}

fn respond_ok(stream: &mut impl Write, extra_headers: &str) {
    let body = br#"{"ok":true,"result":{}}"#;
    write!(
        stream,
        "HTTP/1.1 200 OK\r\n{extra_headers}Content-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(body).unwrap();
    stream.flush().unwrap();
}

/// Whether the peer has finished with the connection; any other read failure, such as a timeout, means it is still open.
fn peer_closed(result: std::io::Result<()>) -> bool {
    match result {
        Ok(()) => false,
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::UnexpectedEof
                    | ErrorKind::ConnectionReset
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::BrokenPipe
            ) =>
        {
            true
        }
        Err(error) => panic!("connection was not closed by the client: {error}"),
    }
}

/// Reads masked client WebSocket frames until the client closes the lease, returning the opcodes seen.
fn drain_websocket(mut stream: impl Read) -> Vec<u8> {
    let mut opcodes = Vec::new();
    loop {
        let mut head = [0; 2];
        if peer_closed(stream.read_exact(&mut head)) {
            return opcodes;
        }
        assert_ne!(head[1] & 0x80, 0, "client WebSocket frames are masked");
        let length = usize::from(head[1] & 0x7f);
        assert!(length < 126, "lease traffic is limited to short frames");
        let mut payload = vec![0; 4 + length];
        if peer_closed(stream.read_exact(&mut payload)) {
            return opcodes;
        }
        let opcode = head[0] & 0x0f;
        opcodes.push(opcode);
        if opcode == 0x8 {
            return opcodes;
        }
    }
}

/// How the fake device finishes its multipart response after the last frame.
#[derive(Clone, Copy)]
enum StreamEnd {
    /// Terminate the multipart body, as a device that stops streaming does.
    Terminate,
    /// Begin another part and wait for the client to hang up, as a live stream does.
    Hold,
}

struct VideoRun {
    output: std::process::Output,
    requests: Vec<String>,
    lease_opcodes: Vec<u8>,
}

/// Runs the CLI against a fake device that logs in with a cookie, grants a WebSocket stream lease and serves `frames` as multipart MJPEG, requiring the session cookie on each request after login.
fn invoke_video(command: &[&str], frames: Vec<Vec<u8>>, end: StreamEnd) -> VideoRun {
    let config = tls_config();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    listener.set_nonblocking(true).unwrap();
    let server = thread::spawn(move || {
        let mut requests = Vec::new();

        let mut login = accept_tls(&listener, &config);
        let request = read_request(&mut login);
        assert!(request.starts_with("POST /api/auth/login "));
        requests.push(request);
        respond_ok(
            &mut login,
            &format!("Set-Cookie: {SESSION_COOKIE}; Path=/\r\n"),
        );

        let mut check = accept_tls(&listener, &config);
        let request = read_request(&mut check);
        assert!(request.starts_with("GET /api/auth/check "));
        assert_eq!(header_value(&request, "cookie"), Some(SESSION_COOKIE));
        requests.push(request);
        respond_ok(&mut check, "");

        let mut lease = accept_tls(&listener, &config);
        let request = read_request(&mut lease);
        assert!(request.starts_with("GET /api/ws?stream=true "));
        assert_eq!(header_value(&request, "cookie"), Some(SESSION_COOKIE));
        let key = header_value(&request, "sec-websocket-key").unwrap();
        let accept = STANDARD.encode(sha1(format!("{key}{WEBSOCKET_GUID}").as_bytes()));
        write!(
            lease,
            "HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n"
        )
        .unwrap();
        lease.flush().unwrap();
        requests.push(request);
        let lease = thread::spawn(move || drain_websocket(lease));

        let mut video = accept_tls(&listener, &config);
        let request = read_request(&mut video);
        assert!(request.starts_with("GET /streamer/stream "));
        assert_eq!(header_value(&request, "cookie"), Some(SESSION_COOKIE));
        requests.push(request);
        video
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nConnection: close\r\n\r\n",
            )
            .unwrap();
        for frame in &frames {
            write!(
                video,
                "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
                frame.len()
            )
            .unwrap();
            video.write_all(frame).unwrap();
            video.write_all(b"\r\n").unwrap();
            video.flush().unwrap();
        }
        match end {
            StreamEnd::Terminate => video.write_all(b"--frame--\r\n").unwrap(),
            StreamEnd::Hold => video.write_all(b"--frame\r\n").unwrap(),
        }
        // The client can finish and exit as soon as it has read the final boundary, so there may be no peer left to flush to.
        let _ = video.flush();
        if matches!(end, StreamEnd::Hold) {
            assert!(
                peer_closed(video.read_exact(&mut [0; 1])),
                "client sent data on the video connection"
            );
        }
        (requests, lease.join().unwrap())
    });
    let child = spawn_cli(address.port(), "pikvm", command);
    let output = child.wait_with_output().unwrap();
    let (requests, lease_opcodes) = server.join().unwrap();
    VideoRun {
        output,
        requests,
        lease_opcodes,
    }
}

fn frame_records(output: &std::process::Output) -> Vec<Value> {
    String::from_utf8(output.stdout.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn decoded_frames(records: &[Value]) -> Vec<Vec<u8>> {
    records
        .iter()
        .map(|record| {
            assert_eq!(record["ok"], true);
            assert_eq!(record["frame"]["encoding"], "base64");
            assert_eq!(record["frame"]["media_type"], "image/jpeg");
            STANDARD
                .decode(record["frame"]["data"].as_str().unwrap())
                .unwrap()
        })
        .collect()
}

fn assert_clean_run(run: &VideoRun) {
    assert!(
        run.output.status.success(),
        "{}",
        String::from_utf8_lossy(&run.output.stderr)
    );
    // The only diagnostic a successful video run may emit is the warning for the test's --insecure-tls.
    assert_eq!(
        String::from_utf8_lossy(&run.output.stderr),
        "warning: TLS certificate verification is disabled\n"
    );
    for secret in ["synthetic-password", SESSION_COOKIE] {
        assert!(!String::from_utf8_lossy(&run.output.stdout).contains(secret));
    }
    assert!(run.requests[0].starts_with("POST /api/auth/login "));
    assert!(run.requests[2].starts_with("GET /api/ws?stream=true "));
    assert!(run.requests[3].starts_with("GET /streamer/stream "));
    assert!(
        run.lease_opcodes
            .iter()
            .all(|opcode| matches!(opcode, 0x1 | 0x8)),
        "unexpected lease traffic {:?}",
        run.lease_opcodes
    );
}

#[test]
fn video_delivers_distinct_frames_and_releases_the_lease() {
    let sent: Vec<Vec<u8>> = JPEG_FRAMES
        .iter()
        .map(|frame| STANDARD.decode(frame).unwrap())
        .collect();
    assert_eq!(sent.iter().collect::<HashSet<_>>().len(), sent.len());
    let run = invoke_video(
        &["video", "--frames", &sent.len().to_string()],
        sent.clone(),
        StreamEnd::Hold,
    );
    assert_clean_run(&run);
    let received = decoded_frames(&frame_records(&run.output));
    assert_eq!(received, sent);
    for frame in &received {
        assert!(frame.starts_with(&[0xff, 0xd8]) && frame.ends_with(&[0xff, 0xd9]));
    }
}

#[test]
fn video_ends_when_the_device_stops_streaming() {
    let sent: Vec<Vec<u8>> = JPEG_FRAMES[..2]
        .iter()
        .map(|frame| STANDARD.decode(frame).unwrap())
        .collect();
    let run = invoke_video(&["video"], sent.clone(), StreamEnd::Terminate);
    assert_clean_run(&run);
    assert_eq!(decoded_frames(&frame_records(&run.output)), sent);
}
