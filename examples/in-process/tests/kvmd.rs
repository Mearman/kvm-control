use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::{State, WebSocketUpgrade},
    http::{Request, StatusCode, Uri, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use kvm_core::{Device, DeviceProfile};
use kvm_kvmd::{Config, Kvmd};
use serde_json::json;
use url::Url;

const JPEG: &[u8] = include_bytes!("synthetic.jpg");
type Calls = Arc<Mutex<Vec<String>>>;

async fn authenticate(request: Request<axum::body::Body>, next: Next) -> Response {
    if request
        .headers()
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        != Some("auth_token=synthetic")
    {
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

async fn state(State(calls): State<Calls>, uri: Uri) -> impl IntoResponse {
    calls.lock().unwrap().push(uri.path().into());
    let result = if uri.path().ends_with("/hid") {
        json!({"enabled": true, "online": true, "keyboard": {"online": true}, "mouse": {"online": true, "absolute": true}})
    } else if uri.path().ends_with("/info") {
        json!({"system": {"kvmd": {"version": "0.0.0-synthetic"}}})
    } else {
        json!({"online": true})
    };
    Json(json!({"ok": true, "result": result}))
}

async fn key(State(calls): State<Calls>, uri: Uri) -> impl IntoResponse {
    calls.lock().unwrap().push(uri.to_string());
    Json(json!({"ok": true, "result": {}}))
}

async fn lease(State(calls): State<Calls>, ws: WebSocketUpgrade) -> impl IntoResponse {
    calls.lock().unwrap().push("websocket lease".into());
    ws.on_upgrade(|mut socket| async move { while socket.recv().await.is_some() {} })
}

async fn stream(State(calls): State<Calls>) -> impl IntoResponse {
    calls.lock().unwrap().push("video stream".into());
    let mut body = format!(
        "--frame\r\nContent-Type: image/jpeg\r\nContent-Length: {}\r\n\r\n",
        JPEG.len()
    )
    .into_bytes();
    body.extend_from_slice(JPEG);
    body.extend_from_slice(b"\r\n--frame--\r\n");
    (
        [(
            header::CONTENT_TYPE,
            "multipart/x-mixed-replace; boundary=frame",
        )],
        body,
    )
}

#[tokio::test]
async fn downstream_uses_real_adapter_against_synthetic_https_device() {
    let certificate = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let ca = certificate.cert.pem().into_bytes();
    let tls = RustlsConfig::from_pem(
        ca.clone(),
        certificate.key_pair.serialize_pem().into_bytes(),
    )
    .await
    .unwrap();
    let calls = Calls::default();
    let app = Router::new()
        .route("/api/info", get(state))
        .route("/api/hid", get(state))
        .route("/api/streamer", get(state))
        .route("/api/hid/events/send_key", post(key))
        .route("/api/streamer/snapshot", get(|| async { JPEG }))
        .route("/api/ws", get(lease))
        .route("/streamer/stream", get(stream))
        .route("/api/auth/check", get(state))
        .route_layer(middleware::from_fn(authenticate))
        .route("/api/auth/login", post(login))
        .with_state(calls.clone());
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
    assert!(matches!(
        adapter.status().await,
        Err(kvm_core::Error::Authentication)
    ));
    assert!(matches!(
        adapter.login("test-user", "test-password").await.unwrap(),
        kvm_kvmd::LoginState::Authenticated
    ));
    let device: &dyn Device = &adapter;
    let status = device.status().await.unwrap();
    assert_eq!(
        status.info.value().unwrap().kvmd_version,
        kvm_core::Reported::Supported("0.0.0-synthetic".into())
    );
    // The synthetic device has no ATX, switch, GPIO or media routes.
    assert_eq!(status.atx, kvm_core::Reported::Unsupported);
    assert_eq!(status.media, kvm_core::Reported::Unsupported);
    assert_eq!(device.snapshot().await.unwrap().jpeg, JPEG);
    device.key("KeyA", true).await.unwrap();
    device.release_inputs().await.unwrap();
    let mut video = device.video().await.unwrap();
    assert_eq!(video.next_frame().await.unwrap().unwrap().jpeg, JPEG);
    drop(video);
    let calls = calls.lock().unwrap();
    assert!(
        calls
            .iter()
            .any(|call| call.contains("send_key?key=KeyA&state=true"))
    );
    assert!(
        calls
            .iter()
            .any(|call| call.contains("send_key?key=KeyA&state=false"))
    );
    assert!(calls.iter().any(|call| call == "websocket lease"));
    assert!(calls.iter().any(|call| call == "video stream"));
    server.abort();
}
