use crate::Kvmd;
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
    };
    (device, closed, task)
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
