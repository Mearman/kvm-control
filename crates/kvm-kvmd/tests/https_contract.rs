use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::IntoResponse,
    routing::{any, get, post},
};
use axum_server::tls_rustls::RustlsConfig;
use kvm_core::{
    Capability, Confirmation, Control, ControlOutcome, Device, DeviceProfile, Error, MouseMode,
    PowerAction, Reported,
};
use kvm_kvmd::{Config, Kvmd, LoginState};
use serde_json::json;
use url::Url;

#[derive(Clone, Default)]
struct Calls(
    Arc<Mutex<Vec<String>>>,
    Arc<AtomicBool>,
    Arc<AtomicBool>,
    Arc<tokio::sync::Notify>,
    Arc<tokio::sync::Notify>,
);

async fn gated_input(State(calls): State<Calls>, uri: Uri) -> impl IntoResponse {
    calls.0.lock().unwrap().push(uri.to_string());
    calls.3.notify_one();
    calls.4.notified().await;
    let query = uri.query().unwrap();
    // "ambiguous" fails its release, "late" its press and a move to x = -1 the movement, standing for requests a server may still apply after the timeout.
    if (query.contains("ambiguous") && query.contains("state=false"))
        || (query.contains("late") && query.contains("state=true"))
        || query.contains("to_x=-1")
    {
        return (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"ok": false})),
        );
    }
    (StatusCode::OK, Json(json!({"ok": true, "result": {}})))
}

async fn login(State(calls): State<Calls>, uri: Uri, body: String) -> impl IntoResponse {
    calls
        .0
        .lock()
        .unwrap()
        .push(format!("{} {body}", uri.path()));
    if body.contains("passwd=test-password") {
        calls.1.store(false, Ordering::SeqCst);
    }
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

async fn gl_totp_login(body: String) -> impl IntoResponse {
    assert_eq!(body, "user=test-user&passwd=test-password012345");
    gl_login().await
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

/// Whether a request carries the fixture's session cookie, KVMD headers or Basic credentials, and the session has not been expired by the test.
fn authorised(calls: &Calls, headers: &HeaderMap) -> bool {
    let cookie =
        headers.get(header::COOKIE).and_then(|v| v.to_str().ok()) == Some("auth_token=synthetic");
    let header_auth = headers.get("x-kvmd-user").and_then(|v| v.to_str().ok()) == Some("test-user")
        && headers.get("x-kvmd-passwd").and_then(|v| v.to_str().ok()) == Some("test-password");
    let basic = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        == Some("Basic dGVzdC11c2VyOnRlc3QtcGFzc3dvcmQ=");
    (cookie || header_auth || basic) && !calls.1.load(Ordering::SeqCst)
}

async fn status(State(calls): State<Calls>, uri: Uri, headers: HeaderMap) -> impl IntoResponse {
    calls
        .0
        .lock()
        .unwrap()
        .push(if uri.path() == "/api/hid/events/send_key" {
            uri.to_string()
        } else {
            uri.path().to_string()
        });
    if !authorised(&calls, &headers) {
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

/// Serves a profile's synthetic status fixture (`glstatus` for GLKVM, `pikvmstatus` for PiKVM, and `corestatus` with only the info, HID and streamer routes) and records every request's method and path, so a test can check that reading status sends nothing else.
async fn typed_status(State(calls): State<Calls>, method: Method, uri: Uri) -> impl IntoResponse {
    calls
        .0
        .lock()
        .unwrap()
        .push(format!("{method} {}", uri.path()));
    let (prefix, route) = uri
        .path()
        .trim_start_matches('/')
        .split_once("/api/")
        .unwrap();
    let fixture: serde_json::Value = serde_json::from_str(if prefix == "glstatus" {
        include_str!("fixtures/glkvm-status.json")
    } else {
        include_str!("fixtures/pikvm-status.json")
    })
    .unwrap();
    let routed = prefix != "corestatus" || matches!(route, "info" | "hid" | "streamer");
    match fixture.get(route) {
        Some(state) if method == Method::GET && routed => {
            (StatusCode::OK, Json(json!({"ok": true, "result": state})))
        }
        _ => (
            StatusCode::NOT_FOUND,
            Json(json!({"ok": false, "result": {}})),
        ),
    }
}

async fn oversized_length() -> impl IntoResponse {
    vec![b' '; 4 * 1024 * 1024 + 1]
}

async fn oversized_chunked() -> axum::body::Body {
    let chunks = (0..65).map(|_| Ok::<_, std::convert::Infallible>(vec![b' '; 64 * 1024]));
    axum::body::Body::from_stream(futures_util::stream::iter(chunks))
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

async fn capability(State(calls): State<Calls>, uri: Uri, headers: HeaderMap) -> impl IntoResponse {
    if !authorised(&calls, &headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"ok": false, "result": {}})),
        );
    }
    let state = match uri.path() {
        "/api/gpio" => {
            json!({"state": {"outputs": {"lamp": {"online": true, "busy": false, "state": true}, "offline": {"online": false, "busy": false, "state": false}, "busy": {"online": true, "busy": true, "state": false}}}})
        }
        "/api/switch" => {
            json!({"model": {"ports": [{"unit": 0, "channel": 0}, {"unit": 0, "channel": 1}]}, "summary": {"active_port": 0, "synced": false}})
        }
        "/api/atx" => json!({"enabled": true, "leds": {"power": false}}),
        _ => json!({"enabled": true, "drive": {"connected": true, "image": {"name": "disk.iso"}}}),
    };
    (StatusCode::OK, Json(json!({"ok": true, "result": state})))
}

async fn switch_fixture(State(calls): State<Calls>, uri: Uri) -> impl IntoResponse {
    let mut log = calls.0.lock().unwrap();
    let reads = log
        .iter()
        .filter(|entry| entry.as_str() == uri.path())
        .count();
    log.push(uri.path().to_owned());
    let ports = if uri.path().starts_with("/malformed/") {
        json!([{}, {"unit": 1, "channel": 1}])
    } else {
        json!([{"unit": 0, "channel": 0}, {"unit": 0, "channel": 1}])
    };
    let synced = uri.path().starts_with("/eventual/") && reads >= 2;
    Json(
        json!({"ok": true, "result": {"model": {"ports": ports}, "summary": {"active_port": if synced {1} else {0}, "synced": synced}, "usb_otg": {"links": [true, false]}}}),
    )
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

// Synthetic HTTPS responses model the authentication and API shapes inspected in the PiKVM kvmd fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef and GLKVM glkvm 3e8dd23c4bd638a4650433664cc3b0f3b4d29395 source revisions. These are not firmware release identifiers or hardware captures.
async fn fixture() -> (Url, Vec<u8>, Calls, tokio::task::JoinHandle<()>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let pem = certified.cert.pem().into_bytes();
    let key = certified.key_pair.serialize_pem().into_bytes();
    let tls = RustlsConfig::from_pem(pem.clone(), key).await.unwrap();
    let calls = Calls::default();
    let app = Router::new()
        .route("/api/auth/login", post(login))
        .route("/long/api/auth/login", post(oversized_length))
        .route("/chunk/api/auth/login", post(login))
        .route("/chunk/api/auth/check", get(oversized_chunked))
        .route("/gl/api/auth/login", post(gl_login))
        .route("/gltotp/api/auth/login", post(gl_totp_login))
        .route("/gltotp/api/auth/two_step_complete", post(gl_complete))
        .route("/gltotp/api/auth/check", get(status))
        .route("/gl/api/auth/two_step_complete", post(gl_complete))
        .route("/gl/api/auth/check", get(status))
        .route("/glgated/api/auth/login", post(gl_login))
        .route("/glgated/api/auth/two_step_complete", post(gl_complete))
        .route("/glgated/api/auth/check", get(status))
        .route("/glgated/api/hid/events/send_key", post(gated_input))
        .route("/bigapproval/api/auth/login", post(gl_login))
        .route(
            "/bigapproval/api/auth/two_step_complete",
            post(oversized_chunked),
        )
        .route("/bigstatus/api/info", get(oversized_length))
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
        .route("/partial/api/hid", get(status))
        .route("/partial/api/info", get(|| async { StatusCode::FORBIDDEN }))
        .route("/partial/api/streamer", get(status))
        .route("/partial/api/atx", get(|| async { StatusCode::FORBIDDEN }))
        .route(
            "/partial/api/switch",
            get(|| async { StatusCode::NOT_FOUND }),
        )
        .route(
            "/partial/api/gpio",
            get(|| async {
                Json(json!({"ok": true, "result": {"state": {"outputs": {"lamp": {}}}}}))
            }),
        )
        .route(
            "/partial/api/msd",
            get(|| async { StatusCode::UNAUTHORIZED }),
        )
        .route("/pikvmstatus/{*route}", any(typed_status))
        .route("/glstatus/{*route}", any(typed_status))
        .route("/corestatus/{*route}", any(typed_status))
        .route("/api/auth/check", get(status))
        .route("/api/info", get(status))
        .route("/api/hid", get(status))
        .route("/api/streamer", get(status))
        .route("/api/poison", get(poisoning_status))
        .route("/api/atx", get(capability))
        .route("/api/switch", get(capability))
        .route("/eventual/api/switch", get(switch_fixture))
        .route("/eventual/api/switch/set_active", post(control))
        .route("/timeout/api/switch", get(switch_fixture))
        .route("/timeout/api/switch/set_active", post(control))
        .route("/malformed/api/switch", get(switch_fixture))
        .route("/malformed/api/switch/set_active", post(control))
        .route("/glswitch/api/switch", get(switch_fixture))
        .route("/glswitch/api/switch/set_active", post(control))
        .route("/api/gpio", get(capability))
        .route("/api/msd", get(capability))
        .route("/gated/api/auth/login", post(login))
        .route("/gated/api/auth/check", get(status))
        .route("/gated/api/hid/events/send_key", post(gated_input))
        .route("/gated/api/hid/events/send_mouse_button", post(gated_input))
        .route("/gated/api/hid", get(status))
        .route("/gated/api/hid/events/send_mouse_move", post(gated_input))
        .route("/api/hid/events/send_key", post(status))
        .route("/api/hid/events/send_mouse_move", post(control))
        .route("/api/atx/power", post(control))
        .route("/api/switch/set_active", post(control))
        .route("/api/gpio/switch", post(control))
        .route("/api/msd/set_connected", post(control))
        .route("/api/msd/set_params", post(control))
        .route(
            "/api/streamer/snapshot",
            get(|State(calls): State<Calls>| async move {
                calls.0.lock().unwrap().push("snapshot".into());
                if calls.2.load(Ordering::SeqCst) {
                    include_bytes!("../../../examples/in-process/tests/synthetic.jpg").to_vec()
                } else {
                    vec![0u8; 16 * 1024 * 1024 + 1]
                }
            }),
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
    profile_client(endpoint, ca, DeviceProfile::PiKvm)
}

/// The fixture client's request timeout, also used as the bound on each wait for the fake device. A single request finishes within it, by reply or by the client's own timeout, so a wait for one request or its outcome that exceeds it means the awaited event did not happen. A wait that starts before anything is sent, or is blocked on the input lock, has no client timeout behind it and relies on the bound alone; and a call that sends several requests can legitimately run past it, which is why `drive` bounds each step rather than the whole call.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// Waits for `future`, failing the test instead of hanging if it does not finish within the client's request timeout.
async fn within<F: std::future::Future>(what: &str, future: F) -> F::Output {
    tokio::time::timeout(REQUEST_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
}

/// Waits for the next gated request to reach the fake device.
async fn arrival(calls: &Calls) {
    within("a request to reach the device", calls.3.notified()).await;
}

fn profile_client(endpoint: Url, ca: Option<Vec<u8>>, profile: DeviceProfile) -> Kvmd {
    Kvmd::new(Config {
        endpoint,
        profile,
        trusted_ca_pem: ca,
        insecure_tls: false,
        timeout: REQUEST_TIMEOUT,
    })
    .unwrap()
}

#[tokio::test]
async fn capabilities_preserve_accessible_features_with_partial_permissions() {
    let (mut endpoint, ca, _, server) = fixture().await;
    endpoint.set_path("/partial/");
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
    let capabilities = device.capabilities().await.unwrap();
    assert_eq!(capabilities.keyboard, Capability::Supported);
    assert_eq!(capabilities.mouse, Capability::Supported);
    assert_eq!(capabilities.status, Capability::Unknown);
    assert_eq!(capabilities.power, Capability::Unknown);
    assert_eq!(capabilities.switch, Capability::Unsupported);
    assert_eq!(capabilities.gpio, Capability::Supported);
    assert_eq!(capabilities.media, Capability::Unknown);
    server.abort();
}

#[tokio::test]
async fn transport_errors_distinguish_timeout_from_untrusted_tls_without_secrets() {
    let (endpoint, ca, _, server) = fixture().await;
    let untrusted = client(endpoint.clone(), None);
    let tls_error = untrusted
        .login("synthetic-user", "synthetic-password")
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(tls_error, "transport error: login: connection failure");

    let timed = Kvmd::new(Config {
        endpoint,
        profile: DeviceProfile::PiKvm,
        trusted_ca_pem: Some(ca),
        insecure_tls: false,
        timeout: Duration::from_millis(50),
    })
    .unwrap();
    let timeout_error = timed
        .login("synthetic-user", "delay")
        .await
        .unwrap_err()
        .to_string();
    assert_eq!(timeout_error, "transport error: login: timeout");
    for error in [tls_error, timeout_error] {
        assert!(!error.contains("localhost"));
        assert!(!error.contains("synthetic"));
    }
    server.abort();
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
    within("the capability request to start", async {
        while !calls
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|v| v == "capability-start")
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
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
    within("the delayed login to start", async {
        while !calls
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|v| v.contains("passwd=delay"))
        {
            tokio::task::yield_now().await;
        }
    })
    .await;
    assert!(matches!(
        device.check_auth().await,
        Err(Error::Authentication)
    ));
    assert!(matches!(task.await.unwrap(), Err(Error::Authentication)));
    server.abort();
}

#[tokio::test]
async fn cancelled_input_is_cleaned_up_with_a_retained_owner() {
    for button in [false, true] {
        let (mut endpoint, ca, calls, server) = fixture().await;
        endpoint.set_path("/gated/");
        let device = Arc::new(client(endpoint, Some(ca)));
        device.login("test-user", "test-password").await.unwrap();
        let sender = device.clone();
        let task = tokio::spawn(async move {
            if button {
                sender.mouse_button("left", true).await
            } else {
                sender.key("KeyA", true).await
            }
        });
        arrival(&calls).await;
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        calls.4.notify_one();
        arrival(&calls).await;
        calls.4.notify_one();
        device.release_inputs().await.unwrap();
        let events: Vec<_> = calls
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v.contains("/hid/events/"))
            .cloned()
            .collect();
        assert_eq!(events.len(), 2);
        assert!(events[0].ends_with("state=true"));
        assert!(events[1].ends_with("state=false"));
        server.abort();
    }
}

/// Polls a request once, so it holds or is queued on the input lock, then spawns it. On any runtime flavour it takes the lock before anything that waits for the lock afterwards; when its spawned work then runs relative to other tasks depends on the runtime.
async fn queue<F>(future: F) -> tokio::task::JoinHandle<F::Output>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    let mut future = Box::pin(future);
    tokio::select! {
        biased;
        _ = &mut future => panic!("request completed before reaching the device"),
        () = std::future::ready(()) => {}
    }
    tokio::spawn(future)
}

/// Completes `future`, letting every gated request reach the device in the order it arrives. Each step, a request arriving or the future finishing, is bounded by the client's request timeout.
async fn drive<F: std::future::Future>(calls: &Calls, future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    loop {
        let step = async {
            tokio::select! {
                biased;
                output = &mut future => Some(output),
                () = calls.3.notified() => None,
            }
        };
        match within("a request or the outcome", step).await {
            Some(output) => return output,
            None => calls.4.notify_one(),
        }
    }
}

/// Records when a manually polled future is woken, which for an input request means its outcome was delivered.
struct Woken(tokio::sync::Notify);

impl std::task::Wake for Woken {
    fn wake(self: Arc<Self>) {
        self.0.notify_one();
    }
}

/// Polls `future` once with a waker that signals `woken`.
fn poll_once<F: std::future::Future + ?Sized>(future: std::pin::Pin<&mut F>, woken: &Arc<Woken>) {
    let waker = std::task::Waker::from(woken.clone());
    assert!(
        future
            .poll(&mut std::task::Context::from_waker(&waker))
            .is_pending()
    );
}

/// Returns the recorded key and button events in order.
fn input_events(calls: &Calls) -> Vec<String> {
    calls
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|v| {
            v.contains("/hid/events/send_key?") || v.contains("/hid/events/send_mouse_button?")
        })
        .map(|v| v.split_once('?').unwrap().1.to_owned())
        .collect()
}

/// Replays the recorded key and button events and returns the inputs the fake device still holds.
fn held_on_device(calls: &Calls) -> Vec<String> {
    let mut held = std::collections::BTreeSet::new();
    for call in calls.0.lock().unwrap().iter() {
        let Some((_, query)) = ["/hid/events/send_key?", "/hid/events/send_mouse_button?"]
            .iter()
            .find_map(|path| call.split_once(path))
        else {
            continue;
        };
        let name = query.split('&').next().unwrap().to_owned();
        if query.ends_with("state=true") {
            held.insert(name);
        } else {
            held.remove(&name);
        }
    }
    held.into_iter().collect()
}

#[tokio::test]
async fn cancelled_cleanup_precedes_later_input_and_explicit_release() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = Arc::new(client(endpoint, Some(ca)));
    let sender = device.clone();
    let first = tokio::spawn(async move { sender.key("KeyA", true).await });
    arrival(&calls).await;
    let sender = device.clone();
    let second = queue(async move { sender.key("KeyB", true).await }).await;
    first.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    calls.4.notify_one();
    arrival(&calls).await;
    assert!(
        calls
            .0
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("key=KeyA&state=false")
    );
    calls.4.notify_one();
    arrival(&calls).await;
    assert!(
        calls
            .0
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("key=KeyB&state=true")
    );
    calls.4.notify_one();
    second.await.unwrap().unwrap();
    let sender = device.clone();
    let release = tokio::spawn(async move { sender.release_inputs().await });
    arrival(&calls).await;
    assert!(
        calls
            .0
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("key=KeyB&state=false")
    );
    calls.4.notify_one();
    release.await.unwrap().unwrap();
    assert_eq!(calls.0.lock().unwrap().len(), 4);
    server.abort();
}

#[tokio::test]
async fn completed_input_awaiting_collection_does_not_hold_the_input_lock() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let mut press = Box::pin(device.key("KeyA", true));
    tokio::select! {
        result = &mut press => panic!("input completed unexpectedly: {result:?}"),
        () = arrival(&calls) => {}
    }
    calls.4.notify_one();
    // The press completes while its future stays alive and unpolled.
    let mut release = Box::pin(device.release_inputs());
    tokio::select! {
        result = &mut release => panic!("release completed unexpectedly: {result:?}"),
        () = arrival(&calls) => {}
    }
    assert!(
        calls
            .0
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .contains("key=KeyA&state=false")
    );
    calls.4.notify_one();
    release.await.unwrap();
    press.await.unwrap();
    assert_eq!(calls.0.lock().unwrap().len(), 2);
    server.abort();
}

#[test]
fn input_future_dropped_outside_runtime_is_cleaned_up() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let mut input = Box::pin(device.key("KeyA", true));
    runtime.block_on(async {
        tokio::select! {
            result = &mut input => panic!("input completed unexpectedly: {result:?}"),
            () = arrival(&calls) => {}
        }
    });
    drop(input);
    runtime.block_on(async {
        calls.4.notify_one();
        arrival(&calls).await;
        assert!(
            calls
                .0
                .lock()
                .unwrap()
                .last()
                .unwrap()
                .contains("key=KeyA&state=false")
        );
        calls.4.notify_one();
        device.release_inputs().await.unwrap();
    });
    server.abort();
}

#[test]
fn input_interrupted_by_runtime_shutdown_stays_tracked_as_ambiguous() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let input_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut input = Box::pin(device.key("KeyA", true));
    input_runtime.block_on(async {
        tokio::select! {
            result = &mut input => panic!("input completed unexpectedly: {result:?}"),
            () = arrival(&calls) => {}
        }
    });
    drop(input);
    // Shutdown drops the input task while its request is still in flight.
    drop(input_runtime);
    assert_eq!(
        calls
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v.contains("/hid/events/"))
            .count(),
        1
    );
    // Releases the request the dropped task left gated on the device, if its handler still runs.
    calls.4.notify_one();
    server_runtime
        .block_on(drive(&calls, device.key("KeyB", true)))
        .unwrap();
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyA&state=false",
            "key=KeyB&state=true"
        ]
    );
    // The interrupted press stays recorded as ambiguous, since the device may still apply it.
    assert!(matches!(
        server_runtime.block_on(drive(&calls, device.release_inputs())),
        Err(Error::AmbiguousInput(_))
    ));
    server.abort();
}

#[tokio::test]
async fn delivered_but_uncollected_input_is_released_before_the_next_input() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut press = Box::pin(device.key("KeyA", true));
    poll_once(press.as_mut(), &woken);
    arrival(&calls).await;
    calls.4.notify_one();
    // The outcome is delivered, waking the caller, which drops the future without collecting it.
    within("the outcome to be delivered", woken.0.notified()).await;
    drop(press);
    drive(&calls, device.key("KeyB", true)).await.unwrap();
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyA&state=false",
            "key=KeyB&state=true"
        ]
    );
    server.abort();
}

#[test]
fn owed_release_interrupted_by_runtime_shutdown_is_not_resent() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let input_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut press = Box::pin(device.key("KeyB", true));
    input_runtime.block_on(async {
        drive(&calls, device.key("KeyA", true)).await.unwrap();
        poll_once(press.as_mut(), &woken);
        arrival(&calls).await;
        calls.4.notify_one();
        within("the outcome to be delivered", woken.0.notified()).await;
    });
    // Dropping the delivered press owes its release and schedules it on the input runtime.
    drop(press);
    input_runtime.block_on(arrival(&calls));
    assert_eq!(input_events(&calls).last().unwrap(), "key=KeyB&state=false");
    // Shutdown drops the cleanup with its release in flight, so the release is ambiguous and must not be sent again.
    drop(input_runtime);
    calls.4.notify_one();
    server_runtime
        .block_on(drive(&calls, device.key("KeyC", true)))
        .unwrap();
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyB&state=true",
            "key=KeyB&state=false",
            "key=KeyC&state=true",
        ]
    );
    server.abort();
}

#[tokio::test]
async fn release_after_an_ambiguous_press_is_sent_after_cleanup() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = Arc::new(client(endpoint, Some(ca)));
    let sender = device.clone();
    let press = tokio::spawn(async move { sender.key("late", true).await });
    arrival(&calls).await;
    press.abort();
    assert!(press.await.unwrap_err().is_cancelled());
    calls.4.notify_one();
    // The abandoned press fails ambiguously, so cleanup sends its owed release, but the device may still apply the press afterwards.
    arrival(&calls).await;
    calls.4.notify_one();
    let _ = drive(&calls, device.key("late", false)).await;
    assert_eq!(
        input_events(&calls),
        [
            "key=late&state=true",
            "key=late&state=false",
            "key=late&state=false",
        ]
    );
    server.abort();
}

#[tokio::test]
async fn release_after_cleanup_is_sent_again_in_a_new_login() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = Arc::new(client(endpoint, Some(ca)));
    let sender = device.clone();
    let press = tokio::spawn(async move { sender.key("KeyA", true).await });
    arrival(&calls).await;
    press.abort();
    assert!(press.await.unwrap_err().is_cancelled());
    calls.4.notify_one();
    drive(&calls, async {
        device.login("test-user", "test-password").await.unwrap();
        device.key("KeyA", false).await.unwrap();
    })
    .await;
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyA&state=false",
            "key=KeyA&state=false"
        ]
    );
    server.abort();
}

#[tokio::test]
async fn cancelled_ambiguous_release_is_not_replayed() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = Arc::new(client(endpoint, Some(ca)));
    drive(&calls, device.key("ambiguous", true)).await.unwrap();
    let sender = device.clone();
    let task = tokio::spawn(async move { sender.key("ambiguous", false).await });
    arrival(&calls).await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    calls.4.notify_one();
    // The ambiguous release must not be resent by the abandoned request's cleanup or by the explicit release.
    assert!(matches!(
        drive(&calls, device.release_inputs()).await,
        Err(Error::AmbiguousInput(_))
    ));
    assert_eq!(
        calls
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|v| v.contains("/hid/events/"))
            .count(),
        2
    );
    server.abort();
}

// Pinned to the current-thread runtime: the second request's task must run its settlement after the first is dropped, which a worker thread could run earlier.
#[tokio::test(flavor = "current_thread")]
async fn request_abandoned_during_owed_cleanup_leaves_nothing_held() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = Arc::new(client(endpoint, Some(ca)));
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut first = Box::pin(device.key("KeyA", true));
    poll_once(first.as_mut(), &woken);
    arrival(&calls).await;
    calls.4.notify_one();
    within("the outcome to be delivered", woken.0.notified()).await;
    // The second request takes the input lock before the first is dropped, so its task holds the lock and sends the release the first then owes.
    let sender = device.clone();
    let second = queue(async move { sender.key("KeyB", true).await }).await;
    drop(first);
    arrival(&calls).await;
    assert_eq!(input_events(&calls).last().unwrap(), "key=KeyA&state=false");
    // Abandoning the second request while the owed release is in flight means its own press is never sent, so nothing is left to clean up.
    second.abort();
    assert!(second.await.unwrap_err().is_cancelled());
    calls.4.notify_one();
    drive(&calls, async {
        device.key("KeyC", true).await.unwrap();
        device.key("KeyC", false).await.unwrap();
    })
    .await;
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyA&state=false",
            "key=KeyC&state=true",
            "key=KeyC&state=false",
        ]
    );
    assert_eq!(held_on_device(&calls), Vec::<String>::new());
    server.abort();
}

#[tokio::test]
async fn release_after_cleanup_released_the_input_is_not_resent() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = Arc::new(client(endpoint, Some(ca)));
    let sender = device.clone();
    let press = tokio::spawn(async move { sender.key("KeyA", true).await });
    arrival(&calls).await;
    press.abort();
    assert!(press.await.unwrap_err().is_cancelled());
    calls.4.notify_one();
    arrival(&calls).await;
    calls.4.notify_one();
    drive(&calls, device.key("KeyA", false)).await.unwrap();
    let events: Vec<_> = calls
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v.contains("/hid/events/"))
        .cloned()
        .collect();
    assert_eq!(events.len(), 2, "{events:?}");
    assert!(events[1].ends_with("key=KeyA&state=false"));
    server.abort();
}

#[tokio::test]
async fn abandoned_press_never_undoes_a_later_acknowledged_input() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut press = Box::pin(device.key("KeyA", true));
    poll_once(press.as_mut(), &woken);
    arrival(&calls).await;
    calls.4.notify_one();
    within("the outcome to be delivered", woken.0.notified()).await;
    // Later requests press another input and re-press the abandoned one, and their callers collect success before the drop.
    drive(&calls, async {
        device.key("KeyC", true).await.unwrap();
        device.key("KeyA", false).await.unwrap();
        device.key("KeyA", true).await.unwrap();
    })
    .await;
    drop(press);
    drive(&calls, device.key("KeyD", true)).await.unwrap();
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyC&state=true",
            "key=KeyA&state=false",
            "key=KeyA&state=true",
            "key=KeyD&state=true",
        ]
    );
    server.abort();
}

#[test]
fn release_interrupted_by_runtime_shutdown_is_finished_by_the_next_lock_holder() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let input_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    input_runtime.block_on(drive(&calls, async {
        device.key("KeyA", true).await.unwrap();
        device.key("KeyB", true).await.unwrap();
    }));
    let mut release = Box::pin(device.release_inputs());
    input_runtime.block_on(async {
        tokio::select! {
            result = &mut release => panic!("release completed unexpectedly: {result:?}"),
            () = arrival(&calls) => {}
        }
    });
    // Shutdown drops the release task with the KeyA release in flight and KeyB not yet released.
    drop(release);
    drop(input_runtime);
    calls.4.notify_one();
    server_runtime
        .block_on(drive(&calls, device.key("KeyC", true)))
        .unwrap();
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyB&state=true",
            "key=KeyA&state=false",
            "key=KeyB&state=false",
            "key=KeyC&state=true",
        ]
    );
    server.abort();
}

/// Returns the recorded HID requests (key, button and movement events and HID state reads), in order.
fn movement_events(calls: &Calls) -> Vec<String> {
    calls
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v.starts_with("/gated/api/hid"))
        .map(|v| v.trim_start_matches("/gated/api/").to_owned())
        .collect()
}

#[tokio::test]
async fn owed_release_is_sent_before_mouse_movement() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    device.login("test-user", "test-password").await.unwrap();
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut press = Box::pin(device.key("KeyA", true));
    poll_once(press.as_mut(), &woken);
    arrival(&calls).await;
    calls.4.notify_one();
    within("the outcome to be delivered", woken.0.notified()).await;
    drop(press);
    drive(&calls, device.mouse_move(1, 2)).await.unwrap();
    assert_eq!(
        movement_events(&calls),
        [
            "hid/events/send_key?key=KeyA&state=true",
            "hid/events/send_key?key=KeyA&state=false",
            "hid",
            "hid/events/send_mouse_move?to_x=1&to_y=2",
        ]
    );
    server.abort();
}

#[tokio::test]
async fn failed_mouse_movement_leaves_the_session_indeterminate() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    device.login("test-user", "test-password").await.unwrap();
    assert!(matches!(
        drive(&calls, device.mouse_move(-1, 2)).await,
        Err(Error::AmbiguousInput(_))
    ));
    assert!(matches!(
        device.release_inputs().await,
        Err(Error::AmbiguousInput(_))
    ));
    server.abort();
}

#[test]
fn mouse_movement_interrupted_by_runtime_shutdown_leaves_the_session_indeterminate() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    server_runtime
        .block_on(device.login("test-user", "test-password"))
        .unwrap();
    let input_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let mut movement = Box::pin(device.mouse_move(1, 2));
    input_runtime.block_on(async {
        tokio::select! {
            result = &mut movement => panic!("movement completed unexpectedly: {result:?}"),
            () = arrival(&calls) => {}
        }
    });
    // Shutdown drops the movement task with its request in flight, so the device may still apply it.
    drop(movement);
    drop(input_runtime);
    calls.4.notify_one();
    assert!(matches!(
        server_runtime.block_on(device.release_inputs()),
        Err(Error::AmbiguousInput(_))
    ));
    server.abort();
}

#[tokio::test]
async fn abandoned_repeat_press_keeps_the_earlier_acknowledged_press() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    drive(&calls, device.key("KeyA", true)).await.unwrap();
    // A repeat press of the held input is delivered and then abandoned; the earlier press reported success, so the input must stay held.
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut repeat = Box::pin(device.key("KeyA", true));
    poll_once(repeat.as_mut(), &woken);
    arrival(&calls).await;
    calls.4.notify_one();
    within("the outcome to be delivered", woken.0.notified()).await;
    drop(repeat);
    drive(&calls, device.key("KeyD", true)).await.unwrap();
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyA&state=true",
            "key=KeyD&state=true",
        ]
    );
    server.abort();
}

#[test]
fn uncollected_press_is_released_on_its_own_runtime() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut press = Box::pin(device.key("KeyA", true));
    server_runtime.block_on(async {
        poll_once(press.as_mut(), &woken);
        arrival(&calls).await;
        calls.4.notify_one();
        within("the outcome to be delivered", woken.0.notified()).await;
    });
    // A later request on a short-lived runtime takes the input lock, and that runtime then shuts down.
    let other_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    other_runtime.block_on(drive(&calls, async {
        device.key("KeyB", true).await.unwrap();
        device.key("KeyB", false).await.unwrap();
    }));
    drop(other_runtime);
    // With no later lock holder, only cleanup scheduled on the press's own runtime can send the owed release.
    drop(press);
    server_runtime.block_on(arrival(&calls));
    calls.4.notify_one();
    server_runtime
        .block_on(drive(&calls, device.key("KeyC", true)))
        .unwrap();
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyB&state=true",
            "key=KeyB&state=false",
            "key=KeyA&state=false",
            "key=KeyC&state=true",
        ]
    );
    server.abort();
}

#[test]
fn owner_dropped_in_a_runtime_releases_inputs_there() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    // The press runs on a short-lived runtime, which is the last to take the input lock and then shuts down.
    let input_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    input_runtime
        .block_on(drive(&calls, device.key("KeyA", true)))
        .unwrap();
    drop(input_runtime);
    server_runtime.block_on(async {
        drop(device);
        arrival(&calls).await;
        calls.4.notify_one();
    });
    assert_eq!(
        input_events(&calls),
        ["key=KeyA&state=true", "key=KeyA&state=false"]
    );
    server.abort();
}

// Pinned to the current-thread runtime: the scheduled cleanup task cannot run before the login takes the input lock, so only the login's own settlement can release the press.
#[tokio::test(flavor = "current_thread")]
async fn login_sends_owed_releases_before_checking_tracked_inputs() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut press = Box::pin(device.key("KeyA", true));
    poll_once(press.as_mut(), &woken);
    arrival(&calls).await;
    calls.4.notify_one();
    within("the outcome to be delivered", woken.0.notified()).await;
    drop(press);
    drive(&calls, device.login("test-user", "test-password"))
        .await
        .unwrap();
    assert_eq!(
        input_events(&calls),
        ["key=KeyA&state=true", "key=KeyA&state=false"]
    );
    server.abort();
}

#[tokio::test]
async fn release_after_cleanup_is_sent_again_after_approval() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/glgated/");
    let device = Arc::new(profile_client(endpoint, Some(ca), DeviceProfile::GlKvm));
    assert!(matches!(
        device.login("test-user", "test-password").await.unwrap(),
        LoginState::ApprovalPending { .. }
    ));
    let sender = device.clone();
    let press = tokio::spawn(async move { sender.key("KeyA", true).await });
    arrival(&calls).await;
    press.abort();
    assert!(press.await.unwrap_err().is_cancelled());
    calls.4.notify_one();
    // The abandoned press releases its own input, then approval starts a new session in which a release is sent again.
    drive(&calls, async {
        assert_eq!(
            device.complete_approval().await.unwrap(),
            LoginState::Authenticated
        );
        device.key("KeyA", false).await.unwrap();
    })
    .await;
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyA&state=false",
            "key=KeyA&state=false"
        ]
    );
    server.abort();
}

/// Leaves a delivered press of `KeyA` uncollected, so it owes a release that cleanup scheduled on the current-thread test runtime has not yet started.
async fn owe_key_a_release(calls: &Calls, device: &Kvmd) {
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut press = Box::pin(device.key("KeyA", true));
    poll_once(press.as_mut(), &woken);
    arrival(calls).await;
    calls.4.notify_one();
    within("the outcome to be delivered", woken.0.notified()).await;
}

// Pinned to the current-thread runtime: the cleanup scheduled by the drop cannot take the input lock before the login does, so only the login's settlement can send the owed release.
#[tokio::test(flavor = "current_thread")]
async fn login_cancelled_during_settlement_still_sends_the_owed_release() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    owe_key_a_release(&calls, &device).await;
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut login = Box::pin(device.login_with_totp("test-user", "test-password", None));
    poll_once(login.as_mut(), &woken);
    drop(login);
    drive(&calls, device.key("KeyC", true)).await.unwrap();
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyA&state=false",
            "key=KeyC&state=true",
        ]
    );
    server.abort();
}

// Pinned to the current-thread runtime for the same reason as the login case.
#[tokio::test(flavor = "current_thread")]
async fn approval_cancelled_during_settlement_still_sends_the_owed_release() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/glgated/");
    let device = profile_client(endpoint, Some(ca), DeviceProfile::GlKvm);
    assert!(matches!(
        device.login("test-user", "test-password").await.unwrap(),
        LoginState::ApprovalPending { .. }
    ));
    owe_key_a_release(&calls, &device).await;
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut approval = Box::pin(device.complete_approval());
    poll_once(approval.as_mut(), &woken);
    drop(approval);
    drive(&calls, device.key("KeyC", true)).await.unwrap();
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyA&state=true",
            "key=KeyA&state=false",
            "key=KeyC&state=true",
        ]
    );
    server.abort();
}

#[test]
fn owner_dropped_while_its_runtime_shuts_down_releases_inputs_on_the_last_runtime() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    server_runtime
        .block_on(drive(&calls, device.key("KeyA", true)))
        .unwrap();
    // A task on a short-lived runtime owns the device, so shutdown drops the owner inside that runtime's context while it discards new tasks.
    let owner_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    owner_runtime.spawn(async move {
        let _device = device;
        std::future::pending::<()>().await;
    });
    drop(owner_runtime);
    server_runtime.block_on(async {
        arrival(&calls).await;
        calls.4.notify_one();
    });
    assert_eq!(
        input_events(&calls),
        ["key=KeyA&state=true", "key=KeyA&state=false"]
    );
    server.abort();
}

#[test]
fn owner_dropped_outside_a_runtime_uses_the_runtime_a_login_last_locked_on() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let input_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    input_runtime
        .block_on(drive(&calls, device.key("KeyA", true)))
        .unwrap();
    drop(input_runtime);
    // A login on a live runtime takes the input lock after the press's runtime has shut down.
    assert!(matches!(
        server_runtime.block_on(device.login("test-user", "test-password")),
        Err(Error::AmbiguousInput(_))
    ));
    std::thread::spawn(move || drop(device)).join().unwrap();
    server_runtime.block_on(async {
        arrival(&calls).await;
        calls.4.notify_one();
    });
    assert_eq!(
        input_events(&calls),
        ["key=KeyA&state=true", "key=KeyA&state=false"]
    );
    server.abort();
}

#[tokio::test]
async fn approval_that_changes_nothing_keeps_the_record_of_cleaned_releases() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/glgated/");
    let device = Arc::new(profile_client(endpoint, Some(ca), DeviceProfile::GlKvm));
    let sender = device.clone();
    let press = tokio::spawn(async move { sender.key("KeyA", true).await });
    arrival(&calls).await;
    press.abort();
    assert!(press.await.unwrap_err().is_cancelled());
    calls.4.notify_one();
    // With no approval pending the session is unchanged, so the release cleanup already sent is still not resent.
    drive(&calls, async {
        assert!(matches!(
            device.complete_approval().await,
            Err(Error::InvalidResponse(_))
        ));
        device.key("KeyA", false).await.unwrap();
    })
    .await;
    assert_eq!(
        input_events(&calls),
        ["key=KeyA&state=true", "key=KeyA&state=false"]
    );
    server.abort();
}

#[test]
fn owner_dropped_on_a_runtime_without_drivers_releases_on_the_last_runtime() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    // The last runtime to take the input lock is driven only when the test blocks on it, so a release scheduled there waits.
    let last_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    last_runtime
        .block_on(drive(&calls, device.key("KeyA", true)))
        .unwrap();
    let bare_runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    bare_runtime.block_on(async move {
        drop(device);
        // Depends on the current-thread scheduler, which the builder above pins: its block_on (tokio 1.54.1, runtime/scheduler/current_thread/mod.rs, `block_on`) runs every task already queued before it polls a yielded future again, so a release started here runs and fails during this yield. The assertion holds on any scheduler; only whether the test catches a release started here depends on this.
        tokio::task::yield_now().await;
    });
    drop(bare_runtime);
    last_runtime.block_on(async {
        arrival(&calls).await;
        calls.4.notify_one();
    });
    assert_eq!(
        input_events(&calls),
        ["key=KeyA&state=true", "key=KeyA&state=false"]
    );
    server.abort();
}

#[test]
fn cleanup_for_an_uncollected_press_records_its_runtime() {
    let server_runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let (mut endpoint, ca, calls, server) = server_runtime.block_on(fixture());
    endpoint.set_path("/gated/");
    let device = client(endpoint, Some(ca));
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut press = Box::pin(device.key("KeyB", true));
    server_runtime.block_on(async {
        poll_once(press.as_mut(), &woken);
        arrival(&calls).await;
        calls.4.notify_one();
        within("the outcome to be delivered", woken.0.notified()).await;
    });
    // A later press on a short-lived runtime is the last lock holder to record itself, and that runtime then shuts down.
    let other_runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    other_runtime
        .block_on(drive(&calls, device.key("KeyC", true)))
        .unwrap();
    drop(other_runtime);
    // The uncollected press's cleanup runs on its own live runtime and records it before sending the owed release.
    drop(press);
    server_runtime.block_on(async {
        arrival(&calls).await;
        calls.4.notify_one();
    });
    std::thread::spawn(move || drop(device)).join().unwrap();
    server_runtime.block_on(async {
        arrival(&calls).await;
        calls.4.notify_one();
    });
    assert_eq!(
        input_events(&calls),
        [
            "key=KeyB&state=true",
            "key=KeyC&state=true",
            "key=KeyB&state=false",
            "key=KeyC&state=false",
        ]
    );
    server.abort();
}

// Pinned to the current-thread runtime, as the key case is: the movement's task starts settling only after the press is dropped, so the owed KeyA release is in flight when the movement is abandoned.
#[tokio::test(flavor = "current_thread")]
async fn movement_abandoned_during_owed_cleanup_is_not_sent() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gated/");
    let device = Arc::new(client(endpoint, Some(ca)));
    device.login("test-user", "test-password").await.unwrap();
    let woken = Arc::new(Woken(tokio::sync::Notify::new()));
    let mut press = Box::pin(device.key("KeyA", true));
    poll_once(press.as_mut(), &woken);
    arrival(&calls).await;
    calls.4.notify_one();
    within("the outcome to be delivered", woken.0.notified()).await;
    let mover = device.clone();
    let movement = queue(async move { mover.mouse_move(1, 2).await }).await;
    drop(press);
    arrival(&calls).await;
    movement.abort();
    assert!(movement.await.unwrap_err().is_cancelled());
    calls.4.notify_one();
    drive(&calls, device.key("KeyC", true)).await.unwrap();
    assert_eq!(
        movement_events(&calls),
        [
            "hid/events/send_key?key=KeyA&state=true",
            "hid/events/send_key?key=KeyA&state=false",
            "hid/events/send_key?key=KeyC&state=true",
        ]
    );
    server.abort();
}

#[tokio::test]
async fn expired_session_requires_explicit_reauthentication() {
    let (endpoint, ca, calls, server) = fixture().await;
    let device = client(endpoint, Some(ca));
    device.login("test-user", "test-password").await.unwrap();
    calls.1.store(true, Ordering::SeqCst);
    assert!(matches!(
        device.key("KeyA", true).await,
        Err(Error::AmbiguousInput(_))
    ));
    assert!(matches!(
        device.check_auth().await,
        Err(Error::Authentication)
    ));
    assert!(matches!(
        device.release_inputs().await,
        Err(Error::AmbiguousInput(_))
    ));
    let inputs_before_reauth: Vec<_> = calls
        .0
        .lock()
        .unwrap()
        .iter()
        .filter(|v| v.starts_with("/api/hid/events/send_key?"))
        .cloned()
        .collect();
    assert_eq!(
        inputs_before_reauth,
        [
            "/api/hid/events/send_key?key=KeyA&state=true",
            "/api/hid/events/send_key?key=KeyA&state=false"
        ]
    );
    device.login("test-user", "test-password").await.unwrap();
    device.check_auth().await.unwrap();
    assert!(matches!(
        device.release_inputs().await,
        Err(Error::AmbiguousInput(_))
    ));
    let paths = calls.0.lock().unwrap();
    assert_eq!(
        paths
            .iter()
            .filter(|v| v.starts_with("/api/auth/login "))
            .count(),
        2
    );
    assert_eq!(
        paths
            .iter()
            .filter(|v| v.starts_with("/api/hid/events/send_key?"))
            .count(),
        2
    );
    server.abort();
}

#[tokio::test]
async fn failed_or_pending_relogin_does_not_reuse_previous_cookie() {
    let (mut endpoint, ca, _, server) = fixture().await;
    endpoint.set_path("/proxy/");
    let device = profile_client(endpoint, Some(ca), DeviceProfile::GlKvm);
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
async fn oversized_json_is_rejected_for_length_and_chunked_responses() {
    let (endpoint, ca, _, server) = fixture().await;
    for prefix in ["long", "chunk"] {
        let mut url = endpoint.clone();
        url.set_path(&format!("/{prefix}/"));
        let device = client(url, Some(ca.clone()));
        assert!(
            matches!(device.login("test-user", "test-password").await, Err(Error::InvalidResponse(message)) if message.contains("size limit"))
        );
    }
    let mut url = endpoint.clone();
    url.set_path("/bigapproval/");
    let device = profile_client(url, Some(ca.clone()), DeviceProfile::GlKvm);
    assert!(matches!(
        device.login("test-user", "test-password").await.unwrap(),
        LoginState::ApprovalPending { .. }
    ));
    assert!(
        matches!(device.complete_approval().await, Err(Error::InvalidResponse(message)) if message.contains("size limit"))
    );
    let mut url = endpoint;
    url.set_path("/bigstatus/");
    let device = client(url, Some(ca));
    assert!(
        matches!(device.status().await, Err(Error::InvalidResponse(message)) if message.contains("size limit"))
    );
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
    let device = profile_client(endpoint, Some(ca), DeviceProfile::GlKvm);
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
async fn switch_observation_uses_profile_semantics_and_rejects_unknown_ports() {
    let (endpoint, ca, calls, server) = fixture().await;
    for profile in [DeviceProfile::GlKvm, DeviceProfile::PiKvm] {
        let device = header_client(endpoint.clone(), &ca, profile);
        let outcome = device
            .control(Control::Switch { port: 0 }, Confirmation::new())
            .await
            .unwrap();
        assert_eq!(outcome, ControlOutcome::Unverified);
        assert!(matches!(
            device
                .control(Control::Switch { port: 2 }, Confirmation::new())
                .await,
            Err(Error::Unsupported(_))
        ));
    }
    let paths = calls.0.lock().unwrap();
    assert_eq!(
        paths
            .iter()
            .filter(|path| path.starts_with("/api/switch/set_active?"))
            .count(),
        2
    );
    assert!(paths.iter().all(|path| !path.contains("port=2")));
    server.abort();
}

#[tokio::test]
async fn pikvm_switch_polling_observes_eventual_match_without_replaying() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/eventual/");
    let device = client(endpoint, Some(ca));
    assert_eq!(
        device
            .control(Control::Switch { port: 1 }, Confirmation::new())
            .await
            .unwrap(),
        ControlOutcome::ObservedMatching
    );
    let log = calls.0.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "/eventual/api/switch")
            .count(),
        3
    );
    assert_eq!(
        log.iter()
            .filter(|entry| entry.starts_with("/eventual/api/switch/set_active?"))
            .count(),
        1
    );
    server.abort();
}

#[tokio::test]
async fn pikvm_switch_polling_stops_without_a_match() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/timeout/");
    let device = client(endpoint, Some(ca));
    assert_eq!(
        device
            .control(Control::Switch { port: 1 }, Confirmation::new())
            .await
            .unwrap(),
        ControlOutcome::Unverified
    );
    let log = calls.0.lock().unwrap();
    assert_eq!(
        log.iter()
            .filter(|entry| entry.as_str() == "/timeout/api/switch")
            .count(),
        6
    );
    assert_eq!(
        log.iter()
            .filter(|entry| entry.starts_with("/timeout/api/switch/set_active?"))
            .count(),
        1
    );
    server.abort();
}

#[tokio::test]
async fn gl_switch_channel_does_not_verify_usb_completion() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/glswitch/");
    let device = profile_client(endpoint, Some(ca), DeviceProfile::GlKvm);
    assert_eq!(
        device
            .control(Control::Switch { port: 0 }, Confirmation::new())
            .await
            .unwrap(),
        ControlOutcome::Unverified
    );
    assert_eq!(
        calls
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|entry| entry.starts_with("/glswitch/api/switch/set_active?"))
            .count(),
        1
    );
    server.abort();
}

#[tokio::test]
async fn malformed_switch_inventory_never_sends_a_control_request() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/malformed/");
    for profile in [DeviceProfile::GlKvm, DeviceProfile::PiKvm] {
        let device = profile_client(endpoint.clone(), Some(ca.clone()), profile);
        for port in [0, 1] {
            assert!(matches!(
                device
                    .control(Control::Switch { port }, Confirmation::new())
                    .await,
                Err(Error::Unsupported(_))
            ));
        }
    }
    assert!(
        !calls
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|entry| entry.starts_with("/malformed/api/switch/set_active"))
    );
    server.abort();
}

#[tokio::test]
async fn pikvm_rejects_gl_approval_response() {
    let (mut endpoint, ca, _, server) = fixture().await;
    endpoint.set_path("/gl/");
    let device = client(endpoint, Some(ca));
    assert!(matches!(
        device.login("test-user", "test-password").await,
        Err(Error::InvalidResponse(_))
    ));
    assert!(matches!(
        device.complete_approval().await,
        Err(Error::Unsupported(_))
    ));
    server.abort();
}

#[tokio::test]
async fn gl_approval_is_not_reported_as_authenticated() {
    let (mut endpoint, ca, _, server) = fixture().await;
    endpoint.set_path("/gl/");
    let device = profile_client(endpoint, Some(ca), DeviceProfile::GlKvm);
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
async fn video_capabilities_require_successful_operations() {
    let (endpoint, ca, calls, server) = fixture().await;
    let device = Kvmd::with_header_auth(
        Config {
            endpoint,
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: Some(ca),
            insecure_tls: false,
            timeout: Duration::from_secs(3),
        },
        "test-user",
        "test-password",
    )
    .unwrap();
    let initial = device.capabilities().await.unwrap();
    assert_eq!(initial.snapshot, Capability::Unknown);
    assert_eq!(initial.live_video, Capability::Unknown);
    assert!(
        !calls.0.lock().unwrap().iter().any(|call| call == "snapshot"
            || call.contains("/ws")
            || call.contains("/streamer/stream"))
    );
    assert!(device.snapshot().await.is_err());
    assert!(device.video().await.is_err());
    let failed = device.capabilities().await.unwrap();
    assert_eq!(failed.snapshot, Capability::Unknown);
    assert_eq!(failed.live_video, Capability::Unknown);
    calls.2.store(true, Ordering::SeqCst);
    device.snapshot().await.unwrap();
    let observed = device.capabilities().await.unwrap();
    assert_eq!(observed.snapshot, Capability::Supported);
    assert_eq!(observed.live_video, Capability::Unknown);
    server.abort();
}

#[tokio::test]
async fn gl_totp_can_precede_device_approval() {
    let (mut endpoint, ca, calls, server) = fixture().await;
    endpoint.set_path("/gltotp/");
    let device = Kvmd::new(Config {
        endpoint,
        profile: DeviceProfile::GlKvm,
        trusted_ca_pem: Some(ca),
        insecure_tls: false,
        timeout: Duration::from_secs(5),
    })
    .unwrap();
    assert!(matches!(
        device
            .login_with_totp("test-user", &"x".repeat(58), Some("012345"))
            .await,
        Err(Error::InvalidResponse(_))
    ));
    assert!(calls.0.lock().unwrap().is_empty());
    assert_eq!(
        device
            .login_with_totp("test-user", "test-password", Some("012345"))
            .await
            .unwrap(),
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
    assert_eq!(
        device.status().await.unwrap().hid.capability(),
        Capability::Supported
    );
    server.abort();
}

#[tokio::test]
async fn totp_is_validated_and_appended_to_cookie_login() {
    let (endpoint, ca, calls, server) = fixture().await;
    let device = client(endpoint, Some(ca));
    for code in ["12345", "12345x", "１２３４５６"] {
        assert!(matches!(
            device
                .login_with_totp("test-user", "test-password", Some(code))
                .await,
            Err(Error::InvalidResponse(_))
        ));
    }
    assert!(calls.0.lock().unwrap().is_empty());
    assert_eq!(
        device
            .login_with_totp("test-user", "test-password", Some("012345"))
            .await
            .unwrap(),
        LoginState::Authenticated
    );
    assert!(
        calls
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|call| call == "/api/auth/login user=test-user&passwd=test-password012345")
    );
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
    let hid = status.hid.value().unwrap();
    assert_eq!(hid.online, Reported::Supported(true));
    assert_eq!(
        status.atx.value().unwrap().power_led,
        Reported::Supported(false)
    );
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

const STATUS_ROUTES: [&str; 7] = ["info", "hid", "streamer", "atx", "switch", "gpio", "msd"];

fn status_client(endpoint: &Url, ca: &[u8], prefix: &str, profile: DeviceProfile) -> Kvmd {
    let mut endpoint = endpoint.clone();
    endpoint.set_path(&format!("/{prefix}/"));
    header_client(endpoint, ca, profile)
}

/// A client authenticating with KVMD headers, which the fixture accepts on every request without a login.
fn header_client(endpoint: Url, ca: &[u8], profile: DeviceProfile) -> Kvmd {
    Kvmd::with_header_auth(
        Config {
            endpoint,
            profile,
            trusted_ca_pem: Some(ca.to_vec()),
            insecure_tls: false,
            timeout: REQUEST_TIMEOUT,
        },
        "test-user",
        "test-password",
    )
    .unwrap()
}

#[tokio::test]
async fn typed_status_reads_every_subsystem_with_get_requests_only() {
    let (endpoint, ca, calls, server) = fixture().await;
    for (prefix, profile) in [
        ("pikvmstatus", DeviceProfile::PiKvm),
        ("glstatus", DeviceProfile::GlKvm),
    ] {
        let status = status_client(&endpoint, &ca, prefix, profile)
            .status()
            .await
            .unwrap();
        let info = status.info.value().unwrap();
        assert!(matches!(info.platform, Reported::Supported(_)));
        let switch = status.switch.value().unwrap();
        assert_eq!(switch.ports.value().unwrap().len(), 2);
        let atx = status.atx.value().unwrap();
        let media = status.media.value().unwrap();
        let streamer = status.streamer.value().unwrap();
        match profile {
            DeviceProfile::PiKvm => {
                assert_eq!(atx.power_led, Reported::Supported(true));
                assert_eq!(atx.power_busy, Reported::Supported(false));
                assert_eq!(switch.active_port, Reported::Supported(Some(1)));
                assert_eq!(switch.synced, Reported::Supported(true));
                let drive = media.drive.value().unwrap();
                assert_eq!(
                    drive.image.value().unwrap().as_ref().unwrap().name,
                    "remote.iso"
                );
                assert_eq!(media.partition_drive, Reported::Unsupported);
                let outputs = status.gpio.value().unwrap().outputs.value().unwrap();
                assert_eq!(outputs[1].channel, "relay");
                assert_eq!(outputs[1].switchable, Reported::Supported(true));
                assert_eq!(streamer.clients, Reported::Supported(1));
            }
            DeviceProfile::GlKvm => {
                assert_eq!(atx.power_busy, Reported::Unsupported);
                assert_eq!(switch.active_port, Reported::Supported(None));
                assert_eq!(switch.synced, Reported::Unknown);
                let partition = media.partition_drive.value().unwrap();
                assert_eq!(
                    partition.image.value().unwrap().as_ref().unwrap().name,
                    "sda1"
                );
                assert_eq!(streamer.source, Reported::Unknown);
            }
        }
        let expected: Vec<String> = STATUS_ROUTES
            .iter()
            .map(|route| format!("GET /{prefix}/api/{route}"))
            .collect();
        assert_eq!(*calls.0.lock().unwrap(), expected);
        calls.0.lock().unwrap().clear();
    }
    server.abort();
}

#[tokio::test]
async fn status_reports_absent_subsystems_as_unsupported() {
    let (endpoint, ca, calls, server) = fixture().await;
    let status = status_client(&endpoint, &ca, "corestatus", DeviceProfile::PiKvm)
        .status()
        .await
        .unwrap();
    assert_eq!(status.info.capability(), Capability::Supported);
    assert_eq!(status.hid.capability(), Capability::Supported);
    assert_eq!(status.streamer.capability(), Capability::Supported);
    assert_eq!(status.atx, Reported::Unsupported);
    assert_eq!(status.switch, Reported::Unsupported);
    assert_eq!(status.gpio, Reported::Unsupported);
    assert_eq!(status.media, Reported::Unsupported);
    assert!(
        calls
            .0
            .lock()
            .unwrap()
            .iter()
            .all(|call| call.starts_with("GET "))
    );
    server.abort();
}
