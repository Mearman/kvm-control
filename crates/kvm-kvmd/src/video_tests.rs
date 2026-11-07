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
    jpeg_colour([64, 128, 192])
}

fn jpeg_colour(colour: [u8; 3]) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut bytes)
        .encode(&colour, 1, 1, image::ExtendedColorType::Rgb8)
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
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
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

/// The input test client's request timeout. An input task that has not finished within it would already have failed its request, so waiting longer cannot change a test's result.
const INPUT_TIMEOUT: Duration = Duration::from_secs(3);

/// Polls an input operation once and returns it spawned. Afterwards the operation is waiting for the input lock that `device` shows as held, so it cannot send a request until the holder finishes.
async fn queued<F>(device: &Kvmd, future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut future = Box::pin(future);
    tokio::select! {
        biased;
        _ = &mut future => panic!("operation completed while the input lock was held"),
        () = std::future::ready(()) => {}
    }
    assert!(device.inputs.try_lock().is_err(), "input lock is not held");
    tokio::spawn(future)
}

/// Waits until every input task holding a clone of `inputs` has finished, so any request it would send has already been sent.
async fn input_tasks_finished(inputs: &Arc<tokio::sync::Mutex<crate::Inputs>>) {
    tokio::time::timeout(INPUT_TIMEOUT, async {
        while Arc::strong_count(inputs) > 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("input task did not finish");
}

/// Asserts that no connection has been made to `listener` and not yet accepted, reading the kernel's accept queue without waiting.
fn assert_no_waiting_connection(listener: TcpListener, message: &str) {
    let listener = listener.into_std().unwrap();
    listener.set_nonblocking(true).unwrap();
    assert_eq!(
        listener.accept().map(|_| ()).unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock,
        "{message}"
    );
}

fn input_device(address: std::net::SocketAddr) -> Kvmd {
    let endpoint = Url::parse(&format!("http://{address}/")).unwrap();
    let client = reqwest::Client::builder()
        .timeout(INPUT_TIMEOUT)
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
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: true,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
    }
}

#[tokio::test]
async fn cancelled_press_finishes_before_concurrent_release() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let (checked_tx, checked_rx) = tokio::sync::oneshot::channel();
    let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        let first = read_request(&mut press).await;
        accepted_tx.send(()).unwrap();
        queued_rx.await.unwrap();
        let listener = listener.into_std().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "release arrived before press completion"
        );
        let listener = TcpListener::from_std(listener).unwrap();
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
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: true,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
    });
    let pressing = device.clone();
    let task = tokio::spawn(async move { pressing.key("KeyA", true).await });
    tokio::time::timeout(INPUT_TIMEOUT, accepted_rx)
        .await
        .unwrap()
        .unwrap();
    task.abort();
    let releasing = device.clone();
    let release = queued(&device, async move { releasing.release_inputs().await }).await;
    queued_tx.send(()).unwrap();
    tokio::time::timeout(INPUT_TIMEOUT, checked_rx)
        .await
        .unwrap()
        .unwrap();
    finish_tx.send(()).unwrap();
    release.await.unwrap().unwrap();
    let (first, second, listener) = tokio::time::timeout(INPUT_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert!(first.contains("state=true"));
    assert!(second.contains("state=false"));
    let inputs = device.inputs.clone();
    drop(device);
    input_tasks_finished(&inputs).await;
    assert_no_waiting_connection(listener, "dropping the owner sent a request");
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
        proceed_rx.await.unwrap();
        let listener = listener.into_std().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "button press arrived before key press completion"
        );
        let listener = TcpListener::from_std(listener).unwrap();
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
    tokio::time::timeout(INPUT_TIMEOUT, accepted_rx)
        .await
        .unwrap()
        .unwrap();
    let second_device = device.clone();
    let second = queued(&device, async move {
        second_device.mouse_button("left", true).await
    })
    .await;
    proceed_tx.send(()).unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    tokio::time::timeout(INPUT_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
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
    let inputs = device.inputs.clone();
    drop(device);
    let listener = tokio::time::timeout(INPUT_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    input_tasks_finished(&inputs).await;
    assert_no_waiting_connection(listener, "the ambiguous press was released twice");
}

#[tokio::test]
async fn timed_out_press_remains_indeterminate_after_release() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut device = input_device(listener.local_addr().unwrap());
    // The server never answers the press, so any timeout makes it ambiguous; a short one keeps the test from waiting.
    const UNANSWERED_PRESS_TIMEOUT: Duration = Duration::from_millis(100);
    let client = reqwest::Client::builder()
        .timeout(UNANSWERED_PRESS_TIMEOUT)
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
    tokio::time::timeout(INPUT_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
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
    tokio::time::timeout(INPUT_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn cancelled_move_finishes_before_button_press() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let device = Arc::new(input_device(listener.local_addr().unwrap()));
    let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = tokio::sync::oneshot::channel();
    let (checked_tx, checked_rx) = tokio::sync::oneshot::channel();
    let (queued_tx, queued_rx) = tokio::sync::oneshot::channel();
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
        queued_rx.await.unwrap();
        let listener = listener.into_std().unwrap();
        listener.set_nonblocking(true).unwrap();
        assert_eq!(
            listener.accept().map(|_| ()).unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock,
            "button press arrived before movement completion"
        );
        let listener = TcpListener::from_std(listener).unwrap();
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
    tokio::time::timeout(INPUT_TIMEOUT, accepted_rx)
        .await
        .unwrap()
        .unwrap();
    movement.abort();
    let clicking = device.clone();
    let button = queued(
        &device,
        async move { clicking.mouse_button("left", true).await },
    )
    .await;
    queued_tx.send(()).unwrap();
    tokio::time::timeout(INPUT_TIMEOUT, checked_rx)
        .await
        .unwrap()
        .unwrap();
    finish_tx.send(()).unwrap();
    button.await.unwrap().unwrap();
    tokio::time::timeout(INPUT_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
}

/// Replaces the device's reporter with one that records each report, and returns the record.
fn capture_reports(device: &mut Kvmd) -> Arc<std::sync::Mutex<Vec<String>>> {
    let reports = Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = reports.clone();
    device.reporter = crate::Reporter(Arc::new(move |message: &str| {
        sink.lock().unwrap().push(message.to_owned());
    }));
    reports
}

/// Writes a complete response with a JSON body and closes the connection.
async fn respond(stream: &mut tokio::net::TcpStream, status: &str, body: &[u8]) {
    stream
        .write_all(
            format!(
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    stream.write_all(body).await.unwrap();
}

#[tokio::test]
async fn movement_abandoned_during_its_state_read_is_not_sent_and_its_read_failure_not_reported() {
    // An answer that allows the movement, one that rejects the mouse mode, and a failed read: once the caller has gone during the read, none sends a movement or reports anything. Not covered: a caller leaving after the last abandonment check but before the outcome is delivered, where only the check that the movement was submitted keeps a read failure unreported; that needs a drop from another thread at an exact moment, which no signal here can order.
    for (status, body) in [
        (
            "200 OK",
            &b"{\"ok\":true,\"result\":{\"enabled\":true,\"mouse\":{\"online\":true,\"absolute\":true}}}"[..],
        ),
        (
            "200 OK",
            &b"{\"ok\":true,\"result\":{\"enabled\":true,\"mouse\":{\"online\":true,\"absolute\":false}}}"[..],
        ),
        ("403 Forbidden", &b"{\"ok\":false}"[..]),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut device = input_device(listener.local_addr().unwrap());
        let reports = capture_reports(&mut device);
        let device = Arc::new(device);
        let (read_tx, read_rx) = tokio::sync::oneshot::channel();
        let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut state, _) = listener.accept().await.unwrap();
            assert!(read_request(&mut state).await.contains("/api/hid "));
            read_tx.send(()).unwrap();
            answer_rx.await.unwrap();
            respond(&mut state, status, body).await;
            listener
        });
        let moving = device.clone();
        let movement = tokio::spawn(async move { moving.mouse_move(1, 2).await });
        tokio::time::timeout(INPUT_TIMEOUT, read_rx)
            .await
            .unwrap()
            .unwrap();
        // The caller leaves while the state read is unanswered, after the check that follows settlement.
        movement.abort();
        assert!(movement.await.unwrap_err().is_cancelled());
        answer_tx.send(()).unwrap();
        let listener = tokio::time::timeout(INPUT_TIMEOUT, server)
            .await
            .unwrap()
            .unwrap();
        // Once the input task has finished, any movement it sent is already waiting to be accepted and any report already made.
        input_tasks_finished(&device.inputs).await;
        assert_no_waiting_connection(listener, "an abandoned movement was sent");
        assert_eq!(*reports.lock().unwrap(), Vec::<String>::new(), "{status}");
    }
}

#[tokio::test]
async fn failed_outcome_of_an_abandoned_press_is_reported_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut device = input_device(listener.local_addr().unwrap());
    let reports = capture_reports(&mut device);
    let device = Arc::new(device);
    let (arrived_tx, arrived_rx) = tokio::sync::oneshot::channel();
    let (answer_tx, answer_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        assert!(read_input_request(&mut press).await.contains("state=true"));
        arrived_tx.send(()).unwrap();
        answer_rx.await.unwrap();
        respond(&mut press, "500 Internal Server Error", b"").await;
        // The abandoned press still owes its release, which the device confirms.
        let (mut release, _) = listener.accept().await.unwrap();
        assert!(
            read_input_request(&mut release)
                .await
                .contains("state=false")
        );
        respond(&mut release, "200 OK", b"{\"ok\":true}").await;
    });
    let pressing = device.clone();
    let press = tokio::spawn(async move { pressing.key("KeyA", true).await });
    tokio::time::timeout(INPUT_TIMEOUT, arrived_rx)
        .await
        .unwrap()
        .unwrap();
    press.abort();
    assert!(press.await.unwrap_err().is_cancelled());
    answer_tx.send(()).unwrap();
    tokio::time::timeout(INPUT_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    input_tasks_finished(&device.inputs).await;
    let reports = reports.lock().unwrap().clone();
    assert_eq!(reports.len(), 1, "{reports:?}");
    assert!(
        reports[0].starts_with("Input outcome uncertain for an abandoned request: "),
        "{reports:?}"
    );
}

#[tokio::test]
async fn failed_release_by_an_owner_dropped_on_another_runtime_is_reported_once() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let mut device = input_device(listener.local_addr().unwrap());
    let reports = capture_reports(&mut device);
    let server = tokio::spawn(async move {
        let (mut press, _) = listener.accept().await.unwrap();
        assert!(read_input_request(&mut press).await.contains("state=true"));
        respond(&mut press, "200 OK", b"{\"ok\":true}").await;
        let (mut release, _) = listener.accept().await.unwrap();
        assert!(
            read_input_request(&mut release)
                .await
                .contains("state=false")
        );
        respond(&mut release, "500 Internal Server Error", b"").await;
        listener
    });
    device.key("KeyA", true).await.unwrap();
    let inputs = device.inputs.clone();
    // Dropped on another runtime, the owner schedules the release on this one and a waiting task on its own, which must not report the failure again. That runtime keeps running until both tasks have finished, so the waiting task runs to completion; this runtime keeps running too, since the blocking thread is awaited rather than joined.
    tokio::task::spawn_blocking(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async move {
            drop(device);
            input_tasks_finished(&inputs).await;
        });
    })
    .await
    .unwrap();
    let listener = tokio::time::timeout(INPUT_TIMEOUT, server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        *reports.lock().unwrap(),
        ["Input cleanup uncertain: a release sent by a dropped owner was not confirmed"]
    );
    assert_no_waiting_connection(listener, "the failed release was sent again");
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
    let inputs = device.inputs.clone();
    drop(device);
    let listener = server.await.unwrap();
    input_tasks_finished(&inputs).await;
    assert_no_waiting_connection(listener, "the ambiguous release was replayed");
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
async fn cancelling_snapshot_read_releases_capacity() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (arrived, ready) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        let (mut first, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let n = first.read(&mut request).await.unwrap();
        assert!(
            std::str::from_utf8(&request[..n])
                .unwrap()
                .starts_with("GET /proxy/api/streamer/snapshot")
        );
        first
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n")
            .await
            .unwrap();
        arrived.send(()).unwrap();
        let mut byte = [0; 1];
        match first.read(&mut byte).await {
            Ok(0) => {}
            Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
            result => panic!("expected connection closure after cancellation: {result:?}"),
        }
        let (mut second, _) = listener.accept().await.unwrap();
        let n = second.read(&mut request).await.unwrap();
        assert!(
            std::str::from_utf8(&request[..n])
                .unwrap()
                .starts_with("GET /proxy/api/streamer/snapshot")
        );
        let frame = jpeg();
        second
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    frame.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        second.write_all(&frame).await.unwrap();
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
            timeout: Duration::from_secs(3),
        },
        endpoint,
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
    };
    let mut snapshot = Box::pin(device.snapshot());
    tokio::select! {
        result = &mut snapshot => panic!("snapshot completed early: {result:?}"),
        result = ready => result.unwrap(),
    }
    drop(snapshot);
    assert_eq!(device.snapshot().await.unwrap().jpeg, jpeg());
    server.await.unwrap();
}

fn gated_jpeg() -> Vec<u8> {
    let mut frame = jpeg();
    let payload = b"validation-test-gate";
    let mut segment = vec![0xff, 0xfe];
    segment.extend_from_slice(&u16::try_from(payload.len() + 2).unwrap().to_be_bytes());
    segment.extend_from_slice(payload);
    frame.splice(2..2, segment);
    frame
}

#[tokio::test]
async fn stream_validation_survives_cancelled_next_frame() {
    let frame = gated_jpeg();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *crate::VALIDATION_GATE.lock().unwrap() = Some((started_tx, release_rx));
    let (device, closed, server) = device(part(&frame, true, true)).await;
    let mut video = device.video().await.unwrap();
    let mut next = Box::pin(video.next_frame());
    tokio::select! {
        result = &mut next => panic!("validation finished before cancellation: {result:?}"),
        result = started_rx => result.unwrap(),
    }
    drop(next);
    release_tx.send(()).unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, frame);
    assert!(video.next_frame().await.unwrap().is_none());
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    *crate::VALIDATION_GATE.lock().unwrap() = Some((started_tx, release_rx));
    let expected = gated_jpeg();
    let sent = expected.clone();
    let snapshot_server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = [0; 4096];
        let n = socket.read(&mut request).await.unwrap();
        assert!(
            std::str::from_utf8(&request[..n])
                .unwrap()
                .starts_with("GET /proxy/api/streamer/snapshot")
        );
        socket
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    sent.len()
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        socket.write_all(&sent).await.unwrap();
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
            timeout: Duration::from_secs(3),
        },
        endpoint,
        profile: DeviceProfile::PiKvm,
        inputs: Default::default(),
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
    };
    let mut snapshot = Box::pin(device.snapshot());
    tokio::select! {
        result = &mut snapshot => panic!("snapshot validation finished before cancellation: {result:?}"),
        result = started_rx => result.unwrap(),
    }
    drop(snapshot);
    release_tx.send(()).unwrap();
    snapshot_server.await.unwrap();
}

#[tokio::test]
async fn concurrent_snapshots_exhaust_and_recover_storage() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let (arrived_tx, mut arrived_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        let mut sockets = Vec::new();
        for _ in 0..crate::MAX_JPEG_VALIDATIONS {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            let n = socket.read(&mut request).await.unwrap();
            assert!(
                std::str::from_utf8(&request[..n])
                    .unwrap()
                    .starts_with("GET /proxy/api/streamer/snapshot")
            );
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 999\r\n\r\n")
                .await
                .unwrap();
            sockets.push(socket);
            arrived_tx.send(()).unwrap();
        }
        for mut socket in sockets {
            let mut byte = [0; 1];
            assert_eq!(socket.read(&mut byte).await.unwrap(), 0);
        }
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
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
    });
    let mut tasks = Vec::new();
    for _ in 0..crate::MAX_JPEG_VALIDATIONS {
        let device = device.clone();
        tasks.push(tokio::spawn(async move { device.snapshot().await }));
        tokio::time::timeout(Duration::from_secs(3), arrived_rx.recv())
            .await
            .unwrap()
            .unwrap();
    }
    assert!(
        matches!(device.snapshot().await, Err(kvm_core::Error::Transport(message)) if message == "snapshot capacity exhausted")
    );
    for task in tasks {
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
    }
    server.await.unwrap();
}

#[tokio::test]
async fn concurrent_streams_exhaust_and_recover_storage() {
    let mut streams = Vec::new();
    let mut servers = Vec::new();
    let slots = Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS));
    for _ in 0..crate::MAX_JPEG_VALIDATIONS {
        let mut body = part(&jpeg(), true, false);
        body.extend_from_slice(b"--frame\r\n");
        let (mut device, closed, server) = device(body).await;
        device.stream_storage = slots.clone();
        let mut stream = device.video().await.unwrap();
        assert_eq!(stream.next_frame().await.unwrap().unwrap().jpeg, jpeg());
        streams.push(stream);
        servers.push((closed, server));
    }
    let mut body = part(&jpeg(), true, false);
    body.extend_from_slice(b"--frame\r\n");
    let (mut extra, closed, server) = device(body).await;
    extra.stream_storage = slots.clone();
    assert!(
        matches!(extra.video().await, Err(kvm_core::Error::Transport(message)) if message == "video stream capacity exhausted")
    );
    drop(streams.pop());
    let (released, completed) = servers.pop().unwrap();
    tokio::time::timeout(Duration::from_secs(3), released.notified())
        .await
        .unwrap();
    completed.await.unwrap();
    let mut replacement = extra.video().await.unwrap();
    assert_eq!(
        replacement.next_frame().await.unwrap().unwrap().jpeg,
        jpeg()
    );
    drop(replacement);
    drop(streams);
    for (closed, server) in servers {
        tokio::time::timeout(Duration::from_secs(3), closed.notified())
            .await
            .unwrap();
        server.await.unwrap();
    }
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    server.await.unwrap();
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
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
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
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
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
        response.extend(part(&jpeg_colour([200, 20, 50]), true, true));
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
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
    };
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    assert_eq!(
        video.next_frame().await.unwrap().unwrap().jpeg,
        jpeg_colour([200, 20, 50])
    );
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
        owed: Default::default(),
        auth_headers: Default::default(),
        approval_token: Default::default(),
        snapshot_proven: Default::default(),
        video_proven: Default::default(),
        cleanup_on_drop: false,
        input_runtime: Default::default(),
        reporter: Default::default(),
        stream_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
        snapshot_storage: Arc::new(tokio::sync::Semaphore::new(crate::MAX_JPEG_VALIDATIONS)),
    };
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg.len(), FRAME);
    assert!(video.next_frame().await.unwrap().is_none());
    server.await.unwrap();
}

// This shared synthetic fixture uses the routes documented for PiKVM kvmd fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef and GLKVM glkvm 3e8dd23c4bd638a4650433664cc3b0f3b4d29395. These are source revisions, not firmware releases, and neither profile is independently verified by this fixture.
#[tokio::test]
async fn explicit_video_reopen_after_complete_or_interrupted_stream() {
    for (profile, interrupted) in [
        (DeviceProfile::PiKvm, false),
        (DeviceProfile::PiKvm, true),
        (DeviceProfile::GlKvm, false),
        (DeviceProfile::GlKvm, true),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            for attempt in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await;
                assert!(request.starts_with("GET /api/ws?stream=true HTTP/1.1\r\n"));
                let key = request
                    .lines()
                    .find_map(|line| {
                        let (name, value) = line.split_once(':')?;
                        name.eq_ignore_ascii_case("sec-websocket-key")
                            .then(|| value.trim())
                    })
                    .unwrap();
                let accept =
                    tokio_tungstenite::tungstenite::handshake::derive_accept_key(key.as_bytes());
                socket.write_all(format!("HTTP/1.1 101 Switching Protocols\r\nConnection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").as_bytes()).await.unwrap();
                let mut ws = tokio_tungstenite::WebSocketStream::from_raw_socket(
                    socket,
                    tokio_tungstenite::tungstenite::protocol::Role::Server,
                    None,
                )
                .await;
                let (mut http, _) = listener.accept().await.unwrap();
                let request = read_request(&mut http).await;
                assert!(request.starts_with("GET /streamer/stream "));
                http.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: multipart/x-mixed-replace; boundary=frame\r\nConnection: close\r\n\r\n").await.unwrap();
                if interrupted && attempt == 0 {
                    http.write_all(b"--frame\r\nContent-Type: image/jpeg\r\nContent-Length: 100\r\n\r\npartial").await.unwrap();
                } else {
                    http.write_all(&part(&jpeg(), true, true)).await.unwrap();
                }
                drop(http);
                tokio::time::timeout(Duration::from_secs(3), async {
                    while let Some(message) = ws.next().await {
                        if message.is_err() {
                            break;
                        }
                    }
                })
                .await
                .unwrap();
            }
        });
        let mut device = input_device(address);
        device.profile = profile;
        device.config.profile = profile;
        for attempt in 0..2 {
            let mut video = device.video().await.unwrap();
            if interrupted && attempt == 0 {
                assert!(matches!(
                    video.next_frame().await,
                    Err(kvm_core::Error::InvalidResponse(_))
                ));
                drop(video);
            } else {
                assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
                assert!(video.next_frame().await.unwrap().is_none());
            }
        }
        server.await.unwrap();
    }
}

#[tokio::test]
async fn continuous_frames_and_drop_close_lease() {
    let mut body = part(&jpeg(), true, false);
    body.extend(part(&jpeg_colour([200, 20, 50]), false, false));
    body.extend_from_slice(b"--frame\r\n");
    let (device, closed, server) = device(body).await;
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, jpeg());
    assert_eq!(
        video.next_frame().await.unwrap().unwrap().jpeg,
        jpeg_colour([200, 20, 50])
    );
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
    tokio::time::timeout(Duration::from_secs(3), closed.notified())
        .await
        .unwrap();
    assert!(video.next_frame().await.unwrap().is_none());
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
