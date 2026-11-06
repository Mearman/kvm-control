use std::{
    sync::{Arc, Mutex, atomic::{AtomicBool, Ordering}},
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
use kvm_core::{
    Capability, Confirmation, Control, ControlOutcome, Device, DeviceProfile, Error, MouseMode,
    PowerAction,
};
use kvm_kvmd::{Config, Kvmd, LoginState};
use serde_json::json;
use url::Url;

#[derive(Clone, Default)]
struct Calls(Arc<Mutex<Vec<String>>>, Arc<AtomicBool>, Arc<AtomicBool>);

async fn login(State(calls): State<Calls>, uri: Uri, body: String) -> impl IntoResponse {
    calls
        .0
        .lock()
        .unwrap()
        .push(format!("{} {body}", uri.path()));
    if body.contains("passwd=bad") {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::SET_COOKIE,
            "auth_token=synthetic; Path=/proxy; Secure; HttpOnly"
                .parse()
                .unwrap(),
        );
        return (
            StatusCode::FORBIDDEN,
            headers,
            Json(json!({"ok": false, "result": {}})),
        );
    }
    if body.contains("passwd=delay") {
        tokio::time::sleep(Duration::from_millis(200)).await;
        return (
            StatusCode::FORBIDDEN,
            HeaderMap::new(),
            Json(json!({"ok": false, "result": {}})),
        );
    }
    if body.contains("passwd=pending") {
        return (
            StatusCode::OK,
            HeaderMap::new(),
            Json(
                json!({"ok": true, "result": {"two_step_required": true, "two_step_token": "synthetic-approval", "expires_in": 60}}),
            ),
        );
    }
    let mut headers = HeaderMap::new();
    let cookie = if uri.path().starts_with("/proxy/") {
        "auth_token=synthetic; Path=/proxy; Secure; HttpOnly"
    } else {
        "auth_token=synthetic; Path=/; Secure; HttpOnly"
    };
    headers.insert(header::SET_COOKIE, cookie.parse().unwrap());
    (
        StatusCode::OK,
        headers,
        Json(json!({"ok": true, "result": {}})),
    )
}

async fn gl_login() -> impl IntoResponse {
    Json(
        json!({"ok": true, "result": {"two_step_required": true, "two_step_token": "synthetic-approval", "expires_in": 60}}),
    )
}

async fn gl_complete(body: String) -> impl IntoResponse {
    assert_eq!(body, "two_step_token=synthetic-approval");
    (
        [(
            header::SET_COOKIE,
            "auth_token=synthetic; Path=/; Secure; HttpOnly",
        )],
        Json(json!({"ok": true, "result": {"token": "synthetic"}})),
    )
}

async fn status(State(calls): State<Calls>, uri: Uri, headers: HeaderMap) -> impl IntoResponse {
    calls.0.lock().unwrap().push(uri.path().to_string());
    let cookie =
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) == Some("auth_token=synthetic");
    let header_auth = headers.get("x-kvmd-user").and_then(|v| v.to_str().ok()) == Some("test-user")
        && headers.get("x-kvmd-passwd").and_then(|v| v.to_str().ok()) == Some("test-password");
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some("Basic dGVzdC11c2VyOnRlc3QtcGFzc3dvcmQ=");
    if !cookie && !header_auth && !basic {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "result": {}})),
        );
    }
    let result = if uri.path().ends_with("/hid") {
        json!({"enabled": true, "online": true, "keyboard": {"online": true, "outputs": {"available": [], "active": ""}}, "mouse": {"online": true, "absolute": !calls.2.load(Ordering::SeqCst), "outputs": {"available": [], "active": ""}}})
    } else {
        json!({"online": true})
    };
    (StatusCode::OK, Json(json!({"ok": true, "result": result})))
}

async fn poisoning_status() -> impl IntoResponse {
    (
        [(
            header::SET_COOKIE,
            "auth_token=poisoned; Path=/; Secure; HttpOnly",
        )],
        Json(json!({"ok": true, "result": {}})),
    )
}

async fn capability(uri: Uri) -> impl IntoResponse {
    let state = match uri.path() {
        "/api/gpio" => {
            json!({"state": {"outputs": {"lamp": {"online": true, "busy": false, "state": true}, "offline": {"online": false, "busy": false, "state": false}, "busy": {"online": true, "busy": true, "state": false}}}})
        }
        "/api/switch" => {
            json!({"model": {"ports": [{}]}, "summary": {"active_port": 0, "synced": false}})
        }
        "/api/atx" => json!({"enabled": true, "leds": {"power": false}}),
        _ => json!({"enabled": true, "drive": {"connected": true, "image": {"name": "disk.iso"}}}),
    };
    Json(json!({"ok": true, "result": state}))
}

async fn control(State(calls): State<Calls>, uri: Uri) -> impl IntoResponse {
    calls.0.lock().unwrap().push(uri.to_string());
    Json(json!({"ok": true, "result": {}}))
}

async fn locked_capability(State(calls): State<Calls>, headers: HeaderMap) -> impl IntoResponse {
    let authenticated =
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) == Some("auth_token=synthetic");
    calls.0.lock().unwrap().push("capability-start".into());
    tokio::time::sleep(Duration::from_millis(100)).await;
    if !authenticated {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "result": {}})),
        );
    }
    (
        StatusCode::OK,
        Json(json!({"ok": true, "result": {"enabled": true, "leds": {"power": false}}})),
    )
}

async fn locked_control(State(calls): State<Calls>, headers: HeaderMap) -> impl IntoResponse {
    let authenticated =
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) == Some("auth_token=synthetic");
    calls.0.lock().unwrap().push("control-submit".into());
    if authenticated {
        (StatusCode::OK, Json(json!({"ok": true, "result": {}})))
    } else {
        (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "result": {}})),
        )
    }
}

async fn fixture() -> (Url, Vec<u8>, Calls, tokio::task::JoinHandle<()>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let pem = certified.cert.pem().into_bytes();
    let key = certified.key_pair.serialize_pem().into_bytes();
    let tls = RustlsConfig::from_pem(pem.clone(), key).await.unwrap();
    let calls = Calls::default();
    let app = Router::new()
        .route("/api/auth/login", post(login))
        .route("/gl/api/auth/login", post(gl_login))
        .route("/gl/api/auth/two_step_complete", post(gl_complete))
        .route("/gl/api/auth/check", get(status))
        .route("/nocookie/api/auth/login", post(gl_login))
        .route(
            "/nocookie/api/auth/two_step_complete",
            post(|| async { Json(json!({"ok": true, "result": {"token": "synthetic"}})) }),
        )
        .route("/nocookie/api/auth/check", get(status))
        .route("/poison/api/auth/login", post(login))
        .route("/poison/api/auth/check", get(status))
        .route("/poison/api/info", get(poisoning_status))
        .route("/poison/api/hid", get(status))
        .route("/poison/api/streamer", get(status))
        .route("/proxy/api/auth/login", post(login))
        .route("/proxy/api/auth/check", get(status))
        .route("/proxy/api/atx", get(locked_capability))
        .route("/proxy/api/atx/power", post(locked_control))
        .route("/api/auth/check", get(status))
        .route("/api/info", get(status))
        .route("/api/hid", get(status))
        .route("/api/streamer", get(status))
        .route("/api/poison", get(poisoning_status))
        .route("/api/atx", get(capability))
        .route("/api/switch", get(capability))
        .route("/api/gpio", get(capability))
        .route("/api/msd", get(capability))
        .route("/api/hid/events/send_key", post(status))
        .route("/api/hid/events/send_mouse_move", post(control))
        .route("/api/atx/power", post(control))
        .route("/api/switch/set_active", post(control))
        .route("/api/gpio/switch", post(control))
        .route("/api/msd/set_connected", post(control))
        .route("/api/msd/set_params", post(control))
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
async fn basic_auth_and_discovered_control_requests() {
    let (endpoint, ca, calls, server) = fixture().await;
    let device = Kvmd::with_basic_auth(
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
    let capabilities = device.capabilities().await.unwrap();
    assert_eq!(
        (
            capabilities.power,
            capabilities.switch,
            capabilities.gpio,
            capabilities.media
        ),
        (
            Capability::Supported,
            Capability::Supported,
            Capability::Supported,
            Capability::Supported
        )
    );
    for (operation, expected) in [
        (
            Control::Power(PowerAction::Off),
            ControlOutcome::ObservedMatching,
        ),
        (Control::Switch { port: 1 }, ControlOutcome::Unverified),
        (Control::Switch { port: 0 }, ControlOutcome::Unverified),
        (
            Control::Gpio {
                channel: "offline".into(),
                state: false,
            },
            ControlOutcome::Unverified,
        ),
        (
            Control::Gpio {
                channel: "busy".into(),
                state: false,
            },
            ControlOutcome::Unverified,
        ),
        (
            Control::Gpio {
                channel: "lamp".into(),
                state: true,
            },
            ControlOutcome::ObservedMatching,
        ),
        (
            Control::MediaConnect { connected: true },
            ControlOutcome::ObservedMatching,
        ),
        (
            Control::MediaSelect {
                image: "disk.iso".into(),
            },
            ControlOutcome::ObservedMatching,
        ),
    ] {
        assert_eq!(
            device
                .control(operation, Confirmation::new())
                .await
                .unwrap(),
            expected
        );
    }
    let calls = calls.0.lock().unwrap();
    for path in [
        "/api/atx/power?action=off&wait=true",
        "/api/switch/set_active?port=1",
        "/api/gpio/switch?channel=lamp&state=true&wait=true",
        "/api/msd/set_connected?connected=true",
        "/api/msd/set_params?image=disk.iso",
    ] {
        assert!(calls.iter().any(|v| v == path), "missing {path}");
    }
    server.abort();
}

#[tokio::test]
async fn control_keeps_session_across_concurrent_relogin() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/proxy/");
    let device = Arc::new(client(endpoint, Some(ca)));
    device.login("test-user", "test-password").await.unwrap();
    let control_device = device.clone();
    let control_task = tokio::spawn(async move {
        control_device
            .control(Control::Power(PowerAction::Off), Confirmation::new())
            .await
    });
    loop {
        if calls
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|v| v == "capability-start")
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    let login_device = device.clone();
    let login_task = tokio::spawn(async move { login_device.login("test-user", "bad").await });
    assert_eq!(
        control_task.await.unwrap().unwrap(),
        ControlOutcome::ObservedMatching
    );
    assert!(matches!(
        login_task.await.unwrap(),
        Err(Error::Authentication)
    ));
    let calls = calls.0.lock().unwrap();
    let submit = calls.iter().position(|v| v == "control-submit").unwrap();
    let relogin = calls.iter().position(|v| v.contains("passwd=bad")).unwrap();
    assert!(submit < relogin);
    server.abort();
}

#[tokio::test]
async fn request_during_relogin_cannot_use_old_session() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/proxy/");
    let device = Arc::new(client(endpoint, Some(ca)));
    device.login("test-user", "test-password").await.unwrap();
    let task_device = device.clone();
    let task = tokio::spawn(async move { task_device.login("test-user", "delay").await });
    loop {
        if calls
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|v| v.contains("passwd=delay"))
        {
            break;
        }
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        device.check_auth().await,
        Err(Error::Authentication)
    ));
    assert!(matches!(task.await.unwrap(), Err(Error::Authentication)));
    server.abort();
}

#[tokio::test]
async fn failed_or_pending_relogin_does_not_reuse_previous_cookie() {
    let (mut endpoint, ca, _, server) = fixture().await;
    endpoint.set_path("/proxy/");
    let device = client(endpoint, Some(ca));
    device.login("test-user", "test-password").await.unwrap();
    device.check_auth().await.unwrap();
    assert!(matches!(
        device.login("test-user", "bad").await,
        Err(Error::Authentication)
    ));
    assert!(matches!(
        device.check_auth().await,
        Err(Error::Authentication)
    ));
    device.login("test-user", "test-password").await.unwrap();
    assert!(matches!(
        device.login("test-user", "pending").await.unwrap(),
        LoginState::ApprovalPending { .. }
    ));
    assert!(matches!(
        device.check_auth().await,
        Err(Error::Authentication)
    ));
    server.abort();
}

#[tokio::test]
async fn absolute_move_rechecks_firmware_mouse_mode() {
    let (endpoint, ca, calls, server) = fixture().await;
    let device = client(endpoint, Some(ca));
    device.login("test-user", "test-password").await.unwrap();
    assert_eq!(
        device.capabilities().await.unwrap().mouse_mode,
        MouseMode::Absolute
    );
    device.mouse_move(10, 20).await.unwrap();
    calls.2.store(true, Ordering::SeqCst);
    assert_eq!(
        device.capabilities().await.unwrap().mouse_mode,
        MouseMode::Relative
    );
    assert!(matches!(
        device.mouse_move(30, 40).await,
        Err(Error::Unsupported(_))
    ));
    let paths = calls.0.lock().unwrap();
    assert_eq!(
        paths
            .iter()
            .filter(|p| p.starts_with("/api/hid/events/send_mouse_move?"))
            .count(),
        1
    );
    server.abort();
}

#[tokio::test]
async fn ordinary_response_cannot_replace_login_cookie() {
    let (mut endpoint, ca, _, server) = fixture().await;
    endpoint.set_path("/poison/");
    let device = client(endpoint, Some(ca));
    device.login("test-user", "test-password").await.unwrap();
    device.status().await.unwrap();
    device.check_auth().await.unwrap();
    server.abort();
}

#[tokio::test]
async fn approval_token_without_usable_cookie_is_rejected() {
    let (mut endpoint, ca, _, server) = fixture().await;
    endpoint.set_path("/nocookie/");
    let device = client(endpoint, Some(ca));
    assert!(matches!(
        device.login("test-user", "test-password").await.unwrap(),
        LoginState::ApprovalPending { .. }
    ));
    assert!(matches!(
        device.complete_approval().await,
        Err(Error::Authentication)
    ));
    assert!(matches!(
        device.check_auth().await,
        Err(Error::Authentication)
    ));
    server.abort();
}

#[tokio::test]
async fn gl_approval_is_not_reported_as_authenticated() {
    let (mut endpoint, ca, _, server) = fixture().await;
    endpoint.set_path("/gl/");
    let device = client(endpoint, Some(ca));
    assert_eq!(
        device.login("test-user", "test-password").await.unwrap(),
        LoginState::ApprovalPending {
            expires_in: Some(60)
        }
    );
    assert_eq!(
        device.complete_approval().await.unwrap(),
        LoginState::Authenticated
    );
    server.abort();
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
    assert_eq!(capabilities.keyboard, Capability::Supported);
    assert_eq!(capabilities.mouse, Capability::Supported);
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
