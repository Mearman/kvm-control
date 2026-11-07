use crate::{Config, Kvmd};
use futures_util::StreamExt;
use kvm_core::{Device, DeviceProfile};
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    sync::Notify,
};
use url::Url;

fn jpeg() -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut bytes)
        .encode(&[64, 128, 192], 1, 1, image::ExtendedColorType::Rgb8)
        .unwrap();
    bytes
}

fn part(frame: &[u8], length: bool, end: bool) -> Vec<u8> {
    let mut bytes = b"--frame\r\nContent-Type: image/jpeg\r\n".to_vec();
    if length {
        bytes.extend_from_slice(format!("Content-Length: {}\r\n", frame.len()).as_bytes());
    }
    bytes.extend_from_slice(b"\r\n");
    bytes.extend_from_slice(frame);
    bytes.extend_from_slice(if end { b"\r\n--frame--\r\n" } else { b"\r\n" });
    bytes
}

async fn device(body: impl Into<Vec<u8>>) -> (Kvmd, Arc<Notify>, tokio::task::JoinHandle<()>) {
    let body = body.into();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let closed = Arc::new(Notify::new());
    let signal = closed.clone();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut peek = [0; 1024];
        let n = socket.peek(&mut peek).await.unwrap();
        assert!(
            std::str::from_utf8(&peek[..n])
                .unwrap()
                .starts_with("GET /proxy/api/ws?stream=true ")
        );
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let watcher = tokio::spawn(async move {
            while let Some(message) = ws.next().await {
                if message.is_err() {
                    break;
                }
            }
            signal.notify_one();
        });
        let (mut http, _) = listener.accept().await.unwrap();
        let mut request = vec![0; 4096];
        let n = http.read(&mut request).await.unwrap();
        assert!(
            std::str::from_utf8(&request[..n])
                .unwrap()
                .starts_with("GET /proxy/streamer/stream ")
        );
        http.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nConnection: close\r\n\r\n").await.unwrap();
        for chunk in body.chunks(7) {
            http.write_all(chunk).await.unwrap();
            tokio::task::yield_now().await;
        }
        let _ = watcher.await;
    });
    let client = reqwest::Client::builder()
        .http1_only()
        .cookie_store(true)
        .build()
        .unwrap();
    let device = Kvmd {
        client: Arc::new(tokio::sync::RwLock::new(crate::Session {
            client: client.clone(),
            video: client,
            cookies: Default::default(),
        })),
        config: Config {
            endpoint: Url::parse(&format!("http://{address}/proxy/")).unwrap(),
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: None,
            insecure_tls: false,
            timeout: Duration::from_secs(3),
        },
        endpoint: Url::parse(&format!("http://{address}/proxy/")).unwrap(),
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
    };
    (device, closed, task)
}

async fn read_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut bytes = Vec::new();
    let mut chunk = [0; 1024];
    while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
        let count = stream.read(&mut chunk).await.unwrap();
        assert_ne!(count, 0, "request ended before headers");
        bytes.extend_from_slice(&chunk[..count]);
    }
    String::from_utf8(bytes).unwrap()
}

fn input_device(address: std::net::SocketAddr) -> Kvmd {
    let endpoint = Url::parse(&format!("http://{address}/")).unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .http1_only()
        .build()
        .unwrap();
    Kvmd {
        client: Arc::new(tokio::sync::RwLock::new(crate::Session {
            client: client.clone(),
            video: client,
            cookies: Default::default(),
        })),
        config: Config {
            endpoint: endpoint.clone(),
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: None,
            insecure_tls: false,
            timeout: Duration::from_secs(3),
        },
        endpoint,
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: true,
        input_runtime: Default::default(),
    }
}

#[tokio::test]
async fn cancelled_press_finishes_before_concurrent_release() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let (checked_tx, checked_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        let first = read_request(&mut press).await;
        accepted_tx.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "release arrived before press completion"
        );
        checked_tx.send(()).unwrap();
        finish_rx.await.unwrap();
        press
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        let (mut release, _) = listener.accept().await.unwrap();
        let second = read_request(&mut release).await;
        release
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        (first, second, listener)
    });
    let endpoint = Url::parse(&format!("http://{address}/")).unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .http1_only()
        .build()
        .unwrap();
    let device = Arc::new(Kvmd {
        client: Arc::new(tokio::sync::RwLock::new(crate::Session {
            client: client.clone(),
            video: client,
            cookies: Default::default(),
        })),
        config: Config {
            endpoint: endpoint.clone(),
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: None,
            insecure_tls: false,
            timeout: Duration::from_secs(3),
        },
        endpoint,
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: true,
        input_runtime: Default::default(),
    });
    let pressing = device.clone();
    let task = tokio::spawn(async move { pressing.key("KeyA", true).await });
    accepted_rx.await.unwrap();
    task.abort();
    let releasing = device.clone();
    let release = tokio::spawn(async move { releasing.release_inputs().await });
    tokio::task::yield_now().await;
    assert!(!release.is_finished());
    checked_rx.await.unwrap();
    finish_tx.send(()).unwrap();
    release.await.unwrap().unwrap();
    let (first, second, listener) = server.await.unwrap();
    assert!(first.contains("state=true"));
    assert!(second.contains("state=false"));
    drop(device);
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn concurrent_key_and_button_presses_are_serialised() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let device = Arc::new(input_device(listener.local_addr().unwrap()));
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (proceed_tx, proceed_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut first).await.contains("send_key"));
        accepted_tx.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        proceed_rx.await.unwrap();
        first
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        let (mut second, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut second)
                .await
                .contains("send_mouse_button")
        );
        second
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
    });
    let first_device = device.clone();
    let first = tokio::spawn(async move { first_device.key("KeyA", true).await });
    accepted_rx.await.unwrap();
    let second_device = device.clone();
    let second = tokio::spawn(async move { second_device.mouse_button("left", true).await });
    tokio::task::yield_now().await;
    proceed_tx.send(()).unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn ambiguous_press_is_reported_and_last_owner_releases_it() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let device = input_device(listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut press).await.contains("state=true"));
        press.write_all(b"HTTP/1.1 500 Internal Server Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        let (mut release, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut release).await.contains("state=false"));
        release
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        listener
    });
    assert!(matches!(
        device.mouse_button("left", true).await,
        Err(kvm_core::Error::AmbiguousInput(_))
    ));
    drop(device);
    let listener = tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn timed_out_press_remains_indeterminate_after_release() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut device = input_device(listener.local_addr().unwrap());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_millis(100))
        .http1_only()
        .build()
        .unwrap();
    device.client = Arc::new(tokio::sync::RwLock::new(crate::Session {
        client: client.clone(),
        video: client,
        cookies: Default::default(),
    }));
    let (applied_tx, applied_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut press).await.contains("state=true"));
        let (mut release, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut release).await.contains("state=false"));
        release
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        applied_rx.await.unwrap();
        drop(press);
    });
    assert!(matches!(
        device.key("KeyA", true).await,
        Err(kvm_core::Error::AmbiguousInput(_))
    ));
    assert!(matches!(
        device.release_inputs().await,
        Err(kvm_core::Error::AmbiguousInput(_))
    ));
    applied_tx.send(()).unwrap();
    server.await.unwrap();
}

async fn read_input_request(stream: &mut tokio::net::TcpStream) -> String {
    let mut request = Vec::new();
    let mut byte = [0; 1];
    while !request.ends_with(b"\r\n\r\n") {
        stream.read_exact(&mut byte).await.unwrap();
        request.push(byte[0]);
    }
    let request = String::from_utf8(request).unwrap();
    let request_line = request.lines().next().unwrap();
    assert!(request_line.starts_with("POST /api/hid/events/send_key?"));
    assert!(request_line.ends_with(" HTTP/1.1"));
    request
}

#[tokio::test]
async fn relogin_cannot_discard_authentication_with_held_input() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let device = input_device(listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut press).await.contains("state=true"));
        press
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        let (mut release, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut release).await.contains("state=false"));
        release
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
    });
    device.key("KeyA", true).await.unwrap();
    assert!(matches!(
        device.login("test", "test").await,
        Err(kvm_core::Error::AmbiguousInput(_))
    ));
    device.release_inputs().await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_move_finishes_before_button_press() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let device = Arc::new(input_device(listener.local_addr().unwrap()));
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let (checked_tx, checked_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut state, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut state).await.contains("/api/hid "));
        let body = b"{\"ok\":true,\"result\":{\"enabled\":true,\"mouse\":{\"online\":true,\"absolute\":true}}}";
        state
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        state.write_all(body).await.unwrap();
        let (mut movement, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut movement)
                .await
                .contains("send_mouse_move")
        );
        accepted_tx.send(()).unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err()
        );
        checked_tx.send(()).unwrap();
        finish_rx.await.unwrap();
        movement
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        let (mut button, _) = listener.accept().await.unwrap();
        assert!(
            read_request(&mut button)
                .await
                .contains("send_mouse_button")
        );
        button
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
    });
    let moving = device.clone();
    let movement = tokio::spawn(async move { moving.mouse_move(1, 2).await });
    accepted_rx.await.unwrap();
    movement.abort();
    let clicking = device.clone();
    let button = tokio::spawn(async move { clicking.mouse_button("left", true).await });
    checked_rx.await.unwrap();
    finish_tx.send(()).unwrap();
    button.await.unwrap().unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn cancelled_waiter_cannot_replace_cleanup_runtime() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let device = Arc::new(input_device(listener.local_addr().unwrap()));
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        assert!(read_input_request(&mut press).await.contains("state=true"));
        accepted_tx.send(()).unwrap();
        finish_rx.await.unwrap();
        press
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        let (mut release, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut release).await.contains("state=false"));
        release
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
    });
    let pressing = device.clone();
    let press = tokio::spawn(async move { pressing.key("KeyA", true).await });
    accepted_rx.await.unwrap();
    let waiting = device.clone();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let task = tokio::spawn(async move { waiting.release_inputs().await });
            tokio::task::yield_now().await;
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        });
    })
    .join()
    .unwrap();
    finish_tx.send(()).unwrap();
    press.await.unwrap().unwrap();
    std::thread::spawn(move || drop(device)).join().unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn ambiguous_release_is_not_replayed_on_drop() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let device = input_device(listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        assert!(read_input_request(&mut press).await.contains("state=true"));
        press
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        let (mut release, _) = listener.accept().await.unwrap();
        assert!(
            read_input_request(&mut release)
                .await
                .contains("state=false")
        );
        release
            .write_all(b"HTTP/1.1 500 Error\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        listener
    });
    device.key("KeyA", true).await.unwrap();
    assert!(matches!(
        device.release_inputs().await,
        Err(kvm_core::Error::AmbiguousInput(_))
    ));
    drop(device);
    let listener = server.await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(100), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn drop_on_ordinary_thread_uses_captured_runtime() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let device = input_device(listener.local_addr().unwrap());
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        assert!(read_input_request(&mut press).await.contains("state=true"));
        press
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
        let (mut release, _) = listener.accept().await.unwrap();
        assert!(read_request(&mut release).await.contains("state=false"));
        release
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"ok\":true}",
            )
            .await
            .unwrap();
    });
    device.key("KeyA", true).await.unwrap();
    std::thread::spawn(move || drop(device)).join().unwrap();
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn trusted_tls_login_cookie_reaches_lease_and_stream() {
    use tokio_rustls::TlsAcceptor;
    use tokio_rustls::rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer};
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut login = acceptor.accept(socket).await.unwrap();
        let mut request = [0; 4096];
        let n = login.read(&mut request).await.unwrap();
        assert!(
            std::str::from_utf8(&request[..n])
                .unwrap()
                .starts_with("POST /proxy/api/auth/login ")
        );
        login.write_all(b"HTTP/1.1 200 OK\r\nSet-Cookie: auth_token=synthetic; Path=/\r\nContent-Length: 23\r\nConnection: close\r\n\r\n{\"ok\":true,\"result\":{}}").await.unwrap();
        login.shutdown().await.unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut check = acceptor.accept(socket).await.unwrap();
        let mut check_request = [0; 4096];
        let n = check.read(&mut check_request).await.unwrap();
        let check_request = std::str::from_utf8(&check_request[..n]).unwrap();
        assert!(check_request.starts_with("GET /proxy/api/auth/check "));
        assert!(
            check_request
                .to_ascii_lowercase()
                .contains("cookie: auth_token=synthetic")
        );
        check.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 23\r\nConnection: close\r\n\r\n{\"ok\":true,\"result\":{}}").await.unwrap();
        check.shutdown().await.unwrap();
        let (socket, _) = listener.accept().await.unwrap();
        let mut tls = acceptor.accept(socket).await.unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            tls.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        let request = String::from_utf8(request).unwrap();
        assert!(request.starts_with("GET /proxy/api/ws?stream=true "));
        assert!(
            request
                .to_ascii_lowercase()
                .contains("cookie: auth_token=synthetic")
        );
        let key = request
            .lines()
            .find_map(|line| line.strip_prefix("sec-websocket-key: "))
            .unwrap();
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        tls.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSet-Cookie: auth_token=attacker; Path=/\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
        let watcher = tokio::spawn(async move {
            let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                tls,
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                None,
            )
            .await;
            while let Some(message) = ws.next().await {
                if message.is_err() {
                    break;
                }
            }
        });
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(socket).await.unwrap();
        let mut request = [0; 4096];
        let n = stream.read(&mut request).await.unwrap();
        let request = std::str::from_utf8(&request[..n])
            .unwrap()
            .to_ascii_lowercase();
        assert!(request.starts_with("get /proxy/streamer/stream "));
        assert!(request.contains("cookie: auth_token=synthetic"));
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nConnection: close\r\n\r\n" ).await.unwrap();
        stream.write_all(&part(&jpeg(), true, true)).await.unwrap();
        watcher.await.unwrap();
    });
    let device = Kvmd::new(Config {
        endpoint: Url::parse(&format!("https://localhost:{}/proxy/", address.port())).unwrap(),
        profile: DeviceProfile::PiKvm,
        trusted_ca_pem: Some(cert.pem().into_bytes()),
        insecure_tls: false,
        timeout: Duration::from_secs(3),
    })
    .unwrap();
    device.login("test", "synthetic").await.unwrap();
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    assert!(video.next_frame().await.unwrap().is_none());
    task.await.unwrap();
}

async fn video_auth_headers(basic: bool) {
    use tokio_rustls::{
        TlsAcceptor,
        rustls::{ServerConfig, pki_types::PrivatePkcs8KeyDer},
    };
    let rcgen::CertifiedKey { cert, key_pair } =
        rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let tls = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.der().clone()],
            PrivatePkcs8KeyDer::from(key_pair.serialize_der()).into(),
        )
        .unwrap();
    let acceptor = TlsAcceptor::from(Arc::new(tls));
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut tls = acceptor.accept(socket).await.unwrap();
        let mut request = Vec::new();
        let mut byte = [0; 1];
        while !request.ends_with(b"\r\n\r\n") {
            tls.read_exact(&mut byte).await.unwrap();
            request.push(byte[0]);
            assert!(request.len() < 8192);
        }
        let request = String::from_utf8(request).unwrap();
        let headers = request.to_ascii_lowercase();
        if basic {
            assert!(
                headers.contains(
                    "authorization: basic dXNlcjpwYXNz"
                        .to_ascii_lowercase()
                        .as_str()
                )
            );
        } else {
            assert!(headers.contains("x-kvmd-user: user"));
            assert!(headers.contains("x-kvmd-passwd: pass"));
        }
        let key = request
            .lines()
            .find_map(|line| line.strip_prefix("sec-websocket-key: "))
            .unwrap();
        let accept = tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
        tls.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSet-Cookie: auth_token=attacker; Path=/\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
        let watcher = tokio::spawn(async move {
            let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                tls,
                tokio_tungstenite::tungstenite::protocol::Role::Server,
                None,
            )
            .await;
            while let Some(message) = ws.next().await {
                if message.is_err() {
                    break;
                }
            }
        });
        let (socket, _) = listener.accept().await.unwrap();
        let mut stream = acceptor.accept(socket).await.unwrap();
        let mut request = [0; 4096];
        let n = stream.read(&mut request).await.unwrap();
        let request = std::str::from_utf8(&request[..n])
            .unwrap()
            .to_ascii_lowercase();
        if basic {
            assert!(request.contains("authorization: basic dxnlcjpwyxnz"));
        } else {
            assert!(request.contains("x-kvmd-user: user"));
            assert!(request.contains("x-kvmd-passwd: pass"));
        }
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n" ).await.unwrap();
        stream.write_all(&part(&jpeg(), true, true)).await.unwrap();
        watcher.await.unwrap();
    });
    let config = Config {
        endpoint: Url::parse(&format!("https://localhost:{}/proxy/", address.port())).unwrap(),
        profile: DeviceProfile::PiKvm,
        trusted_ca_pem: Some(cert.pem().into_bytes()),
        insecure_tls: false,
        timeout: Duration::from_secs(3),
    };
    let device = if basic {
        Kvmd::with_basic_auth(config, "user", "pass")
    } else {
        Kvmd::with_header_auth(config, "user", "pass")
    }
    .unwrap();
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    assert!(video.next_frame().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn header_auth_reaches_video() {
    video_auth_headers(false).await;
}

#[tokio::test]
async fn basic_auth_reaches_video() {
    video_auth_headers(true).await;
}

#[tokio::test]
async fn cancelled_partial_frame_can_resume() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (release, wait) = tokio::sync::oneshot::channel::<()>();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let watcher = tokio::spawn(async move {
            while let Some(message) = ws.next().await {
                if message.is_err() {
                    break;
                }
            }
        });
        let (mut http, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let n = http.read(&mut request).await.unwrap();
        assert!(n > 0);
        let frame = jpeg();
        http.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n", frame.len()).as_bytes()).await.unwrap();
        http.write_all(&frame[..2]).await.unwrap();
        wait.await.unwrap();
        http.write_all(&frame[2..]).await.unwrap();
        http.write_all(b"\r\n--frame--\r\n").await.unwrap();
        watcher.await.unwrap();
    });
    let client = reqwest::Client::builder().http1_only().build().unwrap();
    let device = Kvmd {
        client: Arc::new(tokio::sync::RwLock::new(crate::Session {
            client: client.clone(),
            video: client,
            cookies: Default::default(),
        })),
        config: Config {
            endpoint: Url::parse(&format!("http://{address}/proxy/")).unwrap(),
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: None,
            insecure_tls: false,
            timeout: Duration::from_secs(3),
        },
        endpoint: Url::parse(&format!("http://{address}/proxy/")).unwrap(),
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
    };
    let mut video = device.video().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), video.next_frame())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    server.await.unwrap();
}

#[tokio::test]
async fn relogin_waits_for_video_establishment() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (stream_arrived, stream_wait) = tokio::sync::oneshot::channel();
    let (release_stream, release_wait) = tokio::sync::oneshot::channel();
    let (login_arrived, mut login_wait) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let watcher = tokio::spawn(async move {
            while let Some(message) = ws.next().await {
                if message.is_err() {
                    break;
                }
            }
        });
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(stream.read(&mut request).await.unwrap() > 0);
        stream_arrived.send(()).unwrap();
        release_wait.await.unwrap();
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n" ).await.unwrap();
        stream.write_all(&part(&jpeg(), true, true)).await.unwrap();
        let (mut login, _) = listener.accept().await.unwrap();
        assert!(login.read(&mut request).await.unwrap() > 0);
        login_arrived.send(()).unwrap();
        login
            .write_all(
                b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        watcher.await.unwrap();
    });
    let client = reqwest::Client::builder().http1_only().build().unwrap();
    let endpoint = Url::parse(&format!("http://{address}/proxy/")).unwrap();
    let device = Arc::new(Kvmd {
        client: Arc::new(tokio::sync::RwLock::new(crate::Session {
            client: client.clone(),
            video: client,
            cookies: Default::default(),
        })),
        config: Config {
            endpoint: endpoint.clone(),
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: None,
            insecure_tls: false,
            timeout: Duration::from_secs(3),
        },
        endpoint,
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
    });
    let video_device = device.clone();
    let video_task = tokio::spawn(async move { video_device.video().await });
    stream_wait.await.unwrap();
    let login_device = device.clone();
    let login_task = tokio::spawn(async move { login_device.login("user", "pass").await });
    tokio::task::yield_now().await;
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut login_wait)
            .await
            .is_err()
    );
    release_stream.send(()).unwrap();
    let mut video = video_task.await.unwrap().unwrap();
    assert!(login_task.await.unwrap().is_err());
    assert!(video.next_frame().await.unwrap().is_some());
    drop(video);
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn coalesced_http_response_delivers_multiple_frames() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let watcher = tokio::spawn(async move {
            while let Some(message) = ws.next().await {
                if message.is_err() {
                    break;
                }
            }
        });
        let (mut http, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(http.read(&mut request).await.unwrap() > 0);
        let mut response =
            b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n"
                .to_vec();
        response.extend(part(&jpeg(), true, false));
        response.extend(part(&jpeg(), true, true));
        http.write_all(&response).await.unwrap();
        watcher.await.unwrap();
    });
    let client = reqwest::Client::builder().http1_only().build().unwrap();
    let device = Kvmd {
        client: Arc::new(tokio::sync::RwLock::new(crate::Session {
            client: client.clone(),
            video: client,
            cookies: Default::default(),
        })),
        config: Config {
            endpoint: Url::parse(&format!("http://{address}/proxy/")).unwrap(),
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: None,
            insecure_tls: false,
            timeout: Duration::from_secs(3),
        },
        endpoint: Url::parse(&format!("http://{address}/proxy/")).unwrap(),
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
    };
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    assert!(video.next_frame().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn maximum_frame_with_maximum_headers_and_boundary() {
    const FRAME: usize = 16 * 1024 * 1024;
    const HEADERS: usize = 16 * 1024;
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut ws = tokio_tungstenite::accept_async(socket).await.unwrap();
        let watcher = tokio::spawn(async move {
            while let Some(message) = ws.next().await {
                if message.is_err() {
                    break;
                }
            }
        });
        let (mut http, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        assert!(http.read(&mut request).await.unwrap() > 0);
        let boundary = "x".repeat(70);
        let mut headers = format!("Content-Type: image/jpeg\r\nContent-Length: {FRAME}\r\nX-Pad: ");
        headers.push_str(&"a".repeat(HEADERS - headers.len() - 4));
        headers.push_str("\r\n\r\n");
        http.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary={boundary}\r\n\r\n--{boundary}\r\n{headers}").as_bytes()).await.unwrap();
        let base = jpeg();
        let mut frame = base[..2].to_vec();
        while FRAME - frame.len() - (base.len() - 2) >= 4 {
            let remaining = FRAME - frame.len() - (base.len() - 2);
            let mut payload = (remaining - 4).min(65533);
            if (1..4).contains(&(remaining - payload - 4)) {
                payload -= 4;
            }
            frame.extend_from_slice(&[0xff, 0xfe]);
            frame.extend_from_slice(&((payload + 2) as u16).to_be_bytes());
            frame.extend(std::iter::repeat_n(b'a', payload));
        }
        assert_eq!(frame.len() + base.len() - 2, FRAME);
        frame.extend_from_slice(&base[2..]);
        http.write_all(&frame).await.unwrap();
        http.write_all(format!("\r\n--{boundary}--\r\n").as_bytes())
            .await
            .unwrap();
        watcher.await.unwrap();
    });
    let client = reqwest::Client::builder().http1_only().build().unwrap();
    let endpoint = Url::parse(&format!("http://{address}/proxy/")).unwrap();
    let device = Kvmd {
        client: Arc::new(tokio::sync::RwLock::new(crate::Session {
            client: client.clone(),
            video: client,
            cookies: Default::default(),
        })),
        config: Config {
            endpoint: endpoint.clone(),
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: None,
            insecure_tls: false,
            timeout: Duration::from_secs(10),
        },
        endpoint,
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
    };
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg.len(), FRAME);
    assert!(video.next_frame().await.unwrap().is_none());
    server.await.unwrap();
}

#[tokio::test]
async fn continuous_frames_and_drop_close_lease() {
    let mut body = part(&jpeg(), true, false);
    body.extend(part(&jpeg(), false, false));
    body.extend_from_slice(b"--frame\r\n");
    let (device, closed, server) = device(body).await;
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    drop(video);
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn final_boundary_ends_stream_and_closes_lease() {
    let (device, closed, server) = device(part(&jpeg(), true, true)).await;
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    assert!(video.next_frame().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn boundary_prefix_inside_frame_is_not_a_delimiter() {
    let mut frame = jpeg();
    let comment = b"\r\n--frame-not-a-boundary";
    let mut segment = vec![0xff, 0xfe];
    segment.extend_from_slice(&((comment.len() + 2) as u16).to_be_bytes());
    segment.extend_from_slice(comment);
    frame.splice(2..2, segment);
    let (device, closed, server) = device(part(&frame, false, true)).await;
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, frame);
    assert!(video.next_frame().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_jpeg_is_rejected() {
    let (device, closed, server) = device(part(&[0xff, 0xd8, 0xff, 0xd9], true, true)).await;
    let mut video = device.video().await.unwrap();
    assert!(matches!(
        video.next_frame().await,
        Err(kvm_core::Error::InvalidResponse(_))
    ));
    drop(video);
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn oversized_claim_is_rejected_and_closes_lease() {
    let (device, closed, server) =
        device(&b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 999999999\r\n\r\n"[..])
            .await;
    let mut video = device.video().await.unwrap();
    assert!(video.next_frame().await.is_err());
    drop(video);
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
}
