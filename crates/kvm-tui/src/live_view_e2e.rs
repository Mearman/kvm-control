//! Live view end to end: the real `kvm_kvmd::Kvmd` adapter talks TLS, cookie authentication, the
//! WebSocket lease and the multipart JPEG stream to a synthetic device, and `run_loop` renders the
//! frames it yields into a `TestBackend` buffer using the character-cell (halfblocks) fallback.
//!
//! Declared from `lib.rs` as `#[cfg(test)] mod live_view_e2e;`.

use super::*;
use axum::{
    Json, Router,
    extract::{State, WebSocketUpgrade},
    http::{Request, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use kvm_core::DeviceProfile;
use kvm_kvmd::{Config, Kvmd, LoginState};
use ratatui::{backend::TestBackend, style::Color};
use serde_json::json;
use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};
use tokio::sync::mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel};
use url::Url;

/// Each frame is dominated by a different colour channel, so no two frames can be confused.
const FRAME_COLOURS: [[u8; 3]; 3] = [[220, 10, 10], [10, 200, 10], [10, 10, 220]];
const FRAME_EDGE: u32 = 160;

#[derive(Clone)]
struct Fixture {
    calls: Arc<Mutex<Vec<String>>>,
    frames: Arc<Mutex<Option<UnboundedReceiver<Vec<u8>>>>>,
}

fn encode_frame(colour: [u8; 3]) -> Vec<u8> {
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new(&mut jpeg)
        .encode_image(&image::RgbImage::from_pixel(
            FRAME_EDGE,
            FRAME_EDGE,
            image::Rgb(colour),
        ))
        .unwrap();
    jpeg
}

/// Whether a rendered cell colour is dominated by `channel`. Resampling and JPEG loss shift exact
/// values, but never turn a dominant channel into a minor one.
fn dominated_by(colour: Color, channel: usize) -> bool {
    let Color::Rgb(r, g, b) = colour else {
        return false;
    };
    let channels = [u16::from(r), u16::from(g), u16::from(b)];
    channels[channel] > channels.iter().sum::<u16>() - channels[channel]
}

fn multipart(jpeg: &[u8]) -> Vec<u8> {
    let mut part = format!(
        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        jpeg.len()
    )
    .into_bytes();
    part.extend_from_slice(jpeg);
    part.extend_from_slice(b"\r\n");
    part
}

async fn authenticate(request: Request<axum::body::Body>, next: Next) -> Response {
    let cookie = request
        .headers()
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok());
    if cookie != Some("auth_token=synthetic") {
        return StatusCode::FORBIDDEN.into_response();
    }
    next.run(request).await
}

async fn login(body: String) -> Response {
    if body != "user=test-user&passwd=test-password" {
        return StatusCode::FORBIDDEN.into_response();
    }
    (
        [(
            header::SET_COOKIE,
            "auth_token=synthetic; Path=/; Secure; HttpOnly",
        )],
        Json(json!({"ok": true, "result": {}})),
    )
        .into_response()
}

async fn state(uri: Uri) -> impl IntoResponse {
    let result = if uri.path().ends_with("/hid") {
        json!({"enabled": true, "online": true, "keyboard": {"online": true}, "mouse": {"online": true, "absolute": true}})
    } else {
        json!({"online": true})
    };
    Json(json!({"ok": true, "result": result}))
}

async fn input(State(fixture): State<Fixture>, uri: Uri) -> impl IntoResponse {
    fixture.calls.lock().unwrap().push(uri.to_string());
    Json(json!({"ok": true, "result": {}}))
}

async fn lease(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
}

/// Streams each JPEG the test sends, and stays open until the test drops its sender.
async fn stream(State(fixture): State<Fixture>) -> impl IntoResponse {
    let frames = fixture.frames.lock().unwrap().take().unwrap();
    let body = axum::body::Body::from_stream(futures_util::stream::unfold(
        frames,
        |mut frames| async move {
            frames
                .recv()
                .await
                .map(|jpeg| (Ok::<_, Infallible>(multipart(&jpeg)), frames))
        },
    ));
    (
        [(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=frame",
        )],
        body,
    )
}

struct SyntheticDevice {
    adapter: Kvmd,
    calls: Arc<Mutex<Vec<String>>>,
    frames: UnboundedSender<Vec<u8>>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for SyntheticDevice {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn synthetic_device() -> SyntheticDevice {
    // axum-server is built without a provider; install one explicitly so the test does not depend
    // on which provider features other crates in the build enable. A second install is harmless.
    let _ = rustls::crypto::ring::default_provider().install_default();
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let ca = certificate.cert.pem().into_bytes();
    let tls = RustlsConfig::from_pem(
        ca.clone(),
        certificate.key_pair.serialize_pem().into_bytes(),
    )
    .await
    .unwrap();
    let (frames, receiver) = unbounded_channel();
    let fixture = Fixture {
        calls: Arc::default(),
        frames: Arc::new(Mutex::new(Some(receiver))),
    };
    let calls = fixture.calls.clone();
    let app = Router::new()
        .route("/api/info", get(state))
        .route("/api/hid", get(state))
        .route("/api/streamer", get(state))
        .route("/api/hid/events/send_key", post(input))
        .route("/api/hid/events/send_mouse_move", post(input))
        .route("/api/hid/events/send_mouse_button", post(input))
        .route("/api/ws", get(lease))
        .route("/streamer/stream", get(stream))
        .route_layer(middleware::from_fn(authenticate))
        .route("/api/auth/login", post(login))
        .with_state(fixture);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, tls)
            .unwrap()
            .serve(app.into_make_service())
            .await
            .unwrap();
    });
    let adapter = Kvmd::new(Config {
        endpoint: Url::parse(&format!("https://localhost:{port}/")).unwrap(),
        profile: DeviceProfile::PiKvm,
        trusted_ca_pem: Some(ca),
        insecure_tls: false,
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    assert_eq!(
        adapter.login("test-user", "test-password").await.unwrap(),
        LoginState::Authenticated
    );
    SyntheticDevice {
        adapter,
        calls,
        frames,
        server,
    }
}

/// Records terminal mode changes so restoration order can be asserted.
struct RecordedModes(Arc<Mutex<Vec<&'static str>>>);
impl RecordedModes {
    fn note(&self, mode: &'static str) -> io::Result<()> {
        self.0.lock().unwrap().push(mode);
        Ok(())
    }
}
impl TerminalModes for RecordedModes {
    fn raw(&mut self) -> io::Result<()> {
        self.note("raw")
    }
    fn alternate(&mut self) -> io::Result<()> {
        self.note("alternate")
    }
    fn leave(&mut self) -> io::Result<()> {
        self.note("leave")
    }
    fn cooked(&mut self) -> io::Result<()> {
        self.note("cooked")
    }
    fn mouse_on(&mut self) -> io::Result<()> {
        self.note("mouse_on")
    }
    fn mouse_off(&mut self) -> io::Result<()> {
        self.note("mouse_off")
    }
}

fn press(code: KeyCode) -> Event {
    Event::Key(crossterm::event::KeyEvent::new(
        code,
        crossterm::event::KeyModifiers::NONE,
    ))
}

fn mouse(kind: MouseEventKind, column: u16, row: u16) -> Event {
    Event::Mouse(MouseEvent {
        kind,
        column,
        row,
        modifiers: crossterm::event::KeyModifiers::NONE,
    })
}

fn shows(terminal: &Terminal<TestBackend>, channel: usize) -> bool {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .any(|cell| dominated_by(cell.bg, channel))
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_view_renders_distinct_frames_forwards_input_and_restores_terminal() {
    let device = synthetic_device().await;
    let jpegs = FRAME_COLOURS.map(encode_frame);

    let modes = Arc::new(Mutex::new(Vec::new()));
    let mut guard = TerminalGuard::enter(RecordedModes(modes.clone())).unwrap();
    let mut terminal = Terminal::new(TestBackend::new(60, 24)).unwrap();
    // The same layout `run_loop` draws into: a three row header, the video area, a four row footer.
    let picker = ImagePicker::halfblocks();
    let video_area = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(3),
        Constraint::Length(4),
    ])
    .split(terminal.size().unwrap().into())[1];
    let viewport = video_viewport(video_area, FRAME_EDGE, FRAME_EDGE, picker.cell_size).unwrap();
    let centre = (
        viewport.cells.x + viewport.cells.width / 2,
        viewport.cells.y + viewport.cells.height / 2,
    );

    let mut sent = 0;
    let mut seen = 0;
    let mut input_events = vec![
        press(KeyCode::Char('i')),
        press(KeyCode::Char('a')),
        mouse(MouseEventKind::Down(MouseButton::Left), centre.0, centre.1),
        mouse(MouseEventKind::Up(MouseButton::Left), centre.0, centre.1),
        press(KeyCode::F(12)),
        press(KeyCode::Char('q')),
    ]
    .into_iter();
    let mut opened = false;
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        run_loop(&mut terminal, &device.adapter, &picker, false, |terminal| {
            if !opened {
                opened = true;
                return Ok(Some(press(KeyCode::Char('v'))));
            }
            if seen < jpegs.len() {
                // Release one frame at a time and wait for it to reach the buffer before the
                // next, so a stale frame can never satisfy a later check.
                if sent == seen {
                    device.frames.send(jpegs[sent].clone()).unwrap();
                    sent += 1;
                } else if shows(terminal, seen) {
                    seen += 1;
                }
                std::thread::yield_now();
                return Ok(None);
            }
            Ok(input_events.next())
        }),
    )
    .await
    .expect("live view must finish");
    assert_eq!(seen, jpegs.len(), "every frame must reach the buffer");
    let cleanup = cleanup_terminal(result, &device.adapter, &mut guard).await;
    assert!(cleanup.is_ok(), "{cleanup:?}");

    let calls = device.calls.lock().unwrap().clone();
    let has = |needle: &str| calls.iter().any(|call| call.contains(needle));
    assert!(has("send_key?key=KeyA&state=true"), "{calls:?}");
    assert!(has("send_key?key=KeyA&state=false"), "{calls:?}");
    assert!(has("send_mouse_move?to_x="), "{calls:?}");
    assert!(has("send_mouse_button?button=left&state=true"), "{calls:?}");
    assert!(
        has("send_mouse_button?button=left&state=false"),
        "{calls:?}"
    );

    assert_eq!(
        *modes.lock().unwrap(),
        [
            "raw",
            "alternate",
            "mouse_on",
            "mouse_off",
            "leave",
            "cooked"
        ]
    );
}
