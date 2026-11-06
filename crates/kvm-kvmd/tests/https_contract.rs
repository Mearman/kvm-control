use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode, Uri, header},
    response::IntoResponse,
    routing::{get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use kvm_core::{Capability, Device, DeviceProfile, Error};
use kvm_kvmd::{Config, Kvmd};
use serde_json::json;
use url::Url;

#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<String>>>);

async fn login(State(calls): State<Calls>, uri: Uri, body: String) -> impl IntoResponse {
    calls
        .0
        .lock()
        .unwrap()
        .push(format!("{} {body}", uri.path()));
    (
        [(
            header::SET_COOKIE,
            "auth_token=synthetic; Path=/; Secure; HttpOnly",
        )],
        Json(json!({"ok": true, "result": {}})),
    )
}

async fn status(State(calls): State<Calls>, uri: Uri, headers: HeaderMap) -> impl IntoResponse {
    calls.0.lock().unwrap().push(uri.path().to_string());
    let cookie =
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) == Some("auth_token=synthetic");
    let header_auth = headers.get("x-kvmd-user").and_then(|v| v.to_str().ok()) == Some("test-user")
        && headers.get("x-kvmd-passwd").and_then(|v| v.to_str().ok()) == Some("test-password");
    if !cookie && !header_auth {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "result": {}})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({"ok": true, "result": {"online": true}})),
    )
}

async fn fixture() -> (Url, Vec<u8>, Calls, tokio::task::JoinHandle<()>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let pem = certified.cert.pem().into_bytes();
    let key = certified.key_pair.serialize_pem().into_bytes();
    let tls = RustlsConfig::from_pem(pem.clone(), key).await.unwrap();
    let calls = Calls::default();
    let app = Router::new()
        .route("/api/auth/login", post(login))
        .route("/api/auth/check", get(status))
        .route("/api/info", get(status))
        .route("/api/hid", get(status))
        .route("/api/streamer", get(status))
        .route(
            "/api/streamer/snapshot",
            get(|| async { vec![0u8; 16 * 1024 * 1024 + 1] }),
        )
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
    (
        Url::parse(&format!("https://localhost:{port}/")).unwrap(),
        pem,
        calls,
        server,
    )
}

fn client(endpoint: Url, ca: Option<Vec<u8>>) -> Kvmd {
    Kvmd::new(Config {
        endpoint,
        profile: DeviceProfile::PiKvm,
        trusted_ca_pem: ca,
        insecure_tls: false,
        timeout: Duration::from_secs(5),
    })
    .unwrap()
}

#[tokio::test]
async fn snapshot_rejects_oversized_response() {
    let (endpoint, ca, _, server) = fixture().await;
    let device = client(endpoint, Some(ca));
    assert!(
        matches!(device.snapshot().await, Err(Error::InvalidResponse(message)) if message.contains("size limit"))
    );
    server.abort();
}

#[tokio::test]
async fn header_auth_over_verified_tls() {
    let (endpoint, ca, _, server) = fixture().await;
    let device = Kvmd::with_header_auth(
        Config {
            endpoint,
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: Some(ca),
            insecure_tls: false,
            timeout: Duration::from_secs(5),
        },
        "test-user",
        "test-password",
    )
    .unwrap();
    device.check_auth().await.unwrap();
    assert_eq!(device.status().await.unwrap().info["online"], true);
    server.abort();
}

#[tokio::test]
async fn cookie_login_and_status_over_verified_tls() {
    let (endpoint, ca, calls, server) = fixture().await;
    let untrusted = client(endpoint.clone(), None);
    assert!(matches!(
        untrusted.login("test-user", "test-password").await,
        Err(Error::Transport(_))
    ));
    let device = client(endpoint, Some(ca));
    assert!(matches!(device.status().await, Err(Error::Authentication)));
    device.login("test-user", "test-password").await.unwrap();
    let status = device.status().await.unwrap();
    assert_eq!(status.info["online"], true);
    let capabilities = device.capabilities().await.unwrap();
    assert_eq!(capabilities.status, Capability::Supported);
    assert_eq!(capabilities.keyboard, Capability::Unknown);
    let calls = calls.0.lock().unwrap();
    assert!(
        calls
            .iter()
            .any(|call| call == "/api/auth/login user=test-user&passwd=test-password")
    );
    assert!(calls.iter().any(|call| call == "/api/info"));
    assert!(calls.iter().any(|call| call == "/api/hid"));
    assert!(calls.iter().any(|call| call == "/api/streamer"));
    server.abort();
}
