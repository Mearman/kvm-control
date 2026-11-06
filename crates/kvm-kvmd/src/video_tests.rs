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

async fn device(body: &'static [u8]) -> (Kvmd, Arc<Notify>, tokio::task::JoinHandle<()>) {
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
        client: client.clone(),
        video_client: client,
        timeout: Duration::from_secs(3),
        endpoint: Url::parse(&format!("http://{address}/proxy/")).unwrap(),
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        cookies: Default::default(),
    };
    (device, closed, task)
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
        tls.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
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
        stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nConnection: close\r\n\r\n--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\n\xff\xd8\xff\xd9\r\n--frame--\r\n").await.unwrap();
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
    assert_eq!(
        video.next_frame().await.unwrap().unwrap().jpeg,
        b"\xff\xd8\xff\xd9"
    );
    assert!(video.next_frame().await.unwrap().is_none());
    task.await.unwrap();
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
        http.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\n\r\n--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\n\xff\xd8").await.unwrap();
        wait.await.unwrap();
        http.write_all(b"\xff\xd9\r\n--frame--\r\n").await.unwrap();
        watcher.await.unwrap();
    });
    let client = reqwest::Client::builder().http1_only().build().unwrap();
    let device = Kvmd {
        client: client.clone(),
        video_client: client,
        timeout: Duration::from_secs(3),
        endpoint: Url::parse(&format!("http://{address}/proxy/")).unwrap(),
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
    };
    let mut video = device.video().await.unwrap();
    assert!(
        tokio::time::timeout(Duration::from_millis(30), video.next_frame())
            .await
            .is_err()
    );
    release.send(()).unwrap();
    assert_eq!(
        video.next_frame().await.unwrap().unwrap().jpeg,
        b"\xff\xd8\xff\xd9"
    );
    server.await.unwrap();
}

#[tokio::test]
async fn continuous_frames_and_drop_close_lease() {
    let body = b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 5\r\n\r\n\xff\xd8a\xff\xd9\r\n--frame\r\nContent-Type: image/jpeg\r\n\r\n\xff\xd8b\xff\xd9\r\n--frame\r\n";
    let (device, closed, server) = device(body).await;
    let mut video = device.video().await.unwrap();
    assert_eq!(
        video.next_frame().await.unwrap().unwrap().jpeg,
        b"\xff\xd8a\xff\xd9"
    );
    assert_eq!(
        video.next_frame().await.unwrap().unwrap().jpeg,
        b"\xff\xd8b\xff\xd9"
    );
    drop(video);
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn final_boundary_ends_stream_and_closes_lease() {
    let (device, closed, server) = device(b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\n\xff\xd8\xff\xd9\r\n--frame--\r\n").await;
    let mut video = device.video().await.unwrap();
    assert_eq!(
        video.next_frame().await.unwrap().unwrap().jpeg,
        b"\xff\xd8\xff\xd9"
    );
    assert!(video.next_frame().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn boundary_prefix_inside_frame_is_not_a_delimiter() {
    let (device, closed, server) = device(b"--frame\r\nContent-Type: image/jpeg\r\n\r\n\xff\xd8\r\n--frame-not-a-boundary\xff\xd9\r\n--frame--\r\n").await;
    let mut video = device.video().await.unwrap();
    assert_eq!(
        video.next_frame().await.unwrap().unwrap().jpeg,
        b"\xff\xd8\r\n--frame-not-a-boundary\xff\xd9"
    );
    assert!(video.next_frame().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn invalid_jpeg_is_rejected() {
    let (device, closed, server) = device(
        b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 4\r\n\r\nnope\r\n--frame\r\n",
    )
    .await;
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
        device(b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 999999999\r\n\r\n").await;
    let mut video = device.video().await.unwrap();
    assert!(video.next_frame().await.is_err());
    drop(video);
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
}
