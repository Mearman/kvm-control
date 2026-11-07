//! Property tests that drive the adapter through its public API against a synthetic HTTPS device.
//!
//! The device records every request it receives and models only what a property needs: sessions and approvals, the held keys and buttons, and the state that controls report back. Each test owns one server and resets its state per case. These are not firmware captures.

use std::{
    collections::BTreeSet,
    future::Future,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, Method, StatusCode, Uri, header},
    response::{IntoResponse, Response},
};
use axum_server::tls_rustls::RustlsConfig;
use base64::Engine;
use kvm_core::{
    Confirmation, Control, ControlOutcome, Device as _, DeviceProfile, Error, PowerAction,
};
use kvm_kvmd::{Config, Kvmd, LoginState};
use proptest::{collection::vec, prelude::*};
use serde_json::{Value, json};
use tokio::sync::Notify;
use url::Url;

/// Session id the device offers in `Set-Cookie` headers it should never be able to install.
const POISON_SESSION: u64 = u64::MAX;
/// Ports in the synthetic switch inventory.
const SWITCH_PORTS: u32 = 8;
/// Text that appears only in credentials, tokens and cookies, so its presence in an error proves a leak.
const SECRET_MARKERS: [&str; 3] = ["SECRET-", "TOKEN-", "session-"];

type Pair = (String, String);

#[derive(Clone, Debug)]
struct Request {
    method: String,
    target: String,
    path: String,
    query: Vec<Pair>,
    form: Vec<Pair>,
    cookie: Option<String>,
    user: Option<String>,
    passwd: Option<String>,
    authorization: Option<String>,
}

#[derive(Clone, Copy)]
enum LoginMode {
    Accept,
    Reject,
    Pending,
}

/// Lets a test abandon an input call after the device has the request, then let the device answer.
struct Gate {
    /// Signalled when a gated input request has reached the device and been applied.
    arrived: Notify,
    /// Held by a gated input request until the test lets its response out.
    release: Notify,
}

struct FakeDevice {
    /// Requests are served only under `/g<generation>/`, so a request that a previous case's connection sends late reaches a device that no longer exists instead of corrupting the current case.
    generation: u64,
    gate: Arc<Gate>,
    requests: Vec<Request>,
    next_session: u64,
    sessions: BTreeSet<u64>,
    last_issued: Option<u64>,
    login_mode: LoginMode,
    pending_token: Option<String>,
    issued_tokens: u64,
    approved: bool,
    keys: BTreeSet<String>,
    buttons: BTreeSet<String>,
    stray_releases: Vec<String>,
    power: bool,
    active_port: u64,
    relative: bool,
    fail_status: Option<u16>,
    gate_next: bool,
}

impl FakeDevice {
    fn new(generation: u64) -> Self {
        Self {
            generation,
            gate: Arc::new(Gate {
                arrived: Notify::new(),
                release: Notify::new(),
            }),
            requests: Vec::new(),
            next_session: 0,
            sessions: BTreeSet::from([POISON_SESSION]),
            last_issued: None,
            login_mode: LoginMode::Accept,
            pending_token: None,
            issued_tokens: 0,
            approved: false,
            keys: BTreeSet::new(),
            buttons: BTreeSet::new(),
            stray_releases: Vec::new(),
            power: false,
            active_port: 0,
            relative: false,
            fail_status: None,
            gate_next: false,
        }
    }

    fn issue(&mut self) -> u64 {
        let id = self.next_session;
        self.next_session += 1;
        self.sessions.insert(id);
        self.last_issued = Some(id);
        id
    }
}

struct Shared {
    device: Mutex<FakeDevice>,
}

fn pairs(raw: &str) -> Vec<Pair> {
    url::form_urlencoded::parse(raw.as_bytes())
        .into_owned()
        .collect()
}

fn value(pairs: &[Pair], key: &str) -> Option<String> {
    pairs
        .iter()
        .find(|(name, _)| name.as_str() == key)
        .map(|(_, value)| value.clone())
}

fn ok(result: Value) -> Response {
    Json(json!({"ok": true, "result": result})).into_response()
}

fn session_cookie(id: u64) -> String {
    format!("auth_token=session-{id}; Path=/; Secure; HttpOnly")
}

fn with_session(id: u64, result: Value) -> Response {
    (
        [(header::SET_COOKIE, session_cookie(id))],
        Json(json!({"ok": true, "result": result})),
    )
        .into_response()
}

fn denied() -> Response {
    (
        StatusCode::FORBIDDEN,
        Json(json!({"ok": false, "result": {}})),
    )
        .into_response()
}

fn login(device: &mut FakeDevice, form: &[Pair]) -> Response {
    match device.login_mode {
        LoginMode::Accept => {
            let id = device.issue();
            with_session(id, json!({}))
        }
        // A hostile rejection that echoes the submitted password and offers a cookie.
        LoginMode::Reject => (
            StatusCode::FORBIDDEN,
            [(header::SET_COOKIE, session_cookie(POISON_SESSION))],
            Json(
                json!({"ok": false, "result": {"echo": value(form, "passwd"), "token": "TOKEN-hostile"}}),
            ),
        )
            .into_response(),
        LoginMode::Pending => {
            device.issued_tokens += 1;
            let token = format!("TOKEN-{}", device.issued_tokens);
            device.pending_token = Some(token.clone());
            device.approved = false;
            ok(json!({"two_step_required": true, "two_step_token": token, "expires_in": 60}))
        }
    }
}

fn complete(device: &mut FakeDevice, form: &[Pair]) -> Response {
    let token = value(form, "two_step_token");
    if token.is_none() || token != device.pending_token {
        return denied();
    }
    if !device.approved {
        return ok(json!({"status": "pending"}));
    }
    let id = device.issue();
    with_session(id, json!({"token": format!("TOKEN-approved-{id}")}))
}

/// Accepts any session the device has issued, and answers every check with a cookie the client must not adopt.
fn check(device: &FakeDevice, cookie: Option<&str>) -> Response {
    let valid = cookie
        .and_then(|cookie| cookie.strip_prefix("auth_token=session-"))
        .and_then(|id| id.parse::<u64>().ok())
        .is_some_and(|id| device.sessions.contains(&id));
    let status = if valid {
        StatusCode::OK
    } else {
        StatusCode::FORBIDDEN
    };
    (
        status,
        [(header::SET_COOKIE, session_cookie(POISON_SESSION))],
        Json(json!({"ok": valid, "result": {}})),
    )
        .into_response()
}

fn input(device: &mut FakeDevice, query: &[Pair], param: &str) -> (Response, bool) {
    let name = value(query, param).unwrap_or_default();
    let pressed = value(query, "state").as_deref() == Some("true");
    let held = if param == "button" {
        &mut device.buttons
    } else {
        &mut device.keys
    };
    if pressed {
        held.insert(name);
    } else if !held.remove(&name) {
        device.stray_releases.push(format!("{param}:{name}"));
    }
    (ok(json!({})), std::mem::take(&mut device.gate_next))
}

/// Returns the response and whether the request must wait at the gate before it is sent.
fn respond(device: &mut FakeDevice, mut request: Request) -> (Response, bool) {
    let prefix = format!("/g{}", device.generation);
    let path = request
        .path
        .strip_prefix(&prefix)
        .filter(|rest| rest.starts_with('/'))
        .map(str::to_owned);
    let target = request.target.strip_prefix(&prefix).map(str::to_owned);
    let (Some(path), Some(target)) = (path, target) else {
        return (StatusCode::GONE.into_response(), false);
    };
    request.path = path;
    request.target = target;
    device.requests.push(request.clone());
    if let Some(code) = device.fail_status {
        let echo = json!({"ok": false, "result": {"echo": [request.user, request.passwd, request.authorization]}});
        let status = StatusCode::from_u16(code).unwrap();
        return ((status, Json(echo)).into_response(), false);
    }
    let Request {
        method,
        path,
        query,
        form,
        cookie,
        ..
    } = request;
    match (method.as_str(), path.as_str()) {
        ("POST", "/api/auth/login") => (login(device, &form), false),
        ("POST", "/api/auth/two_step_complete") => (complete(device, &form), false),
        ("GET", "/api/auth/check") => (check(device, cookie.as_deref()), false),
        ("GET", "/api/hid") => (
            ok(json!({
                "enabled": true,
                "keyboard": {"online": true},
                "mouse": {"online": true, "absolute": !device.relative},
            })),
            false,
        ),
        ("POST", "/api/hid/events/send_key") => input(device, &query, "key"),
        ("POST", "/api/hid/events/send_mouse_button") => input(device, &query, "button"),
        ("POST", "/api/hid/events/send_mouse_move") => (ok(json!({})), false),
        ("GET", "/api/atx") => (
            ok(json!({"enabled": true, "leds": {"power": device.power}})),
            false,
        ),
        ("POST", "/api/atx/power") => {
            match value(&query, "action").as_deref() {
                Some("on") => device.power = true,
                Some("off" | "off_hard") => device.power = false,
                _ => {}
            }
            (ok(json!({})), false)
        }
        ("GET", "/api/gpio") => (
            ok(
                json!({"state": {"outputs": {"lamp": {"online": true, "busy": false, "state": true}}}}),
            ),
            false,
        ),
        ("POST", "/api/gpio/switch" | "/api/msd/set_connected" | "/api/msd/set_params") => {
            (ok(json!({})), false)
        }
        ("GET", "/api/msd") => (
            ok(
                json!({"enabled": true, "drive": {"connected": true, "image": {"name": "disk.iso"}}}),
            ),
            false,
        ),
        ("GET", "/api/switch") => {
            let ports: Vec<Value> = (0..SWITCH_PORTS)
                .map(|port| json!({"unit": port / 4, "channel": port % 4}))
                .collect();
            (
                ok(json!({
                    "model": {"ports": ports},
                    "summary": {"active_port": device.active_port, "synced": true},
                })),
                false,
            )
        }
        ("POST", "/api/switch/set_active") => {
            if let Some(port) = value(&query, "port").and_then(|port| port.parse::<u64>().ok()) {
                device.active_port = port;
            }
            (ok(json!({})), false)
        }
        _ => (StatusCode::NOT_FOUND.into_response(), false),
    }
}

async fn handle(
    State(shared): State<Arc<Shared>>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    let header = |name: &str| {
        headers
            .get(name)
            .map(|value| String::from_utf8_lossy(value.as_bytes()).into_owned())
    };
    let request = Request {
        method: method.to_string(),
        target: uri.to_string(),
        path: uri.path().to_owned(),
        query: pairs(uri.query().unwrap_or_default()),
        form: pairs(&body),
        cookie: header("cookie"),
        user: header("x-kvmd-user"),
        passwd: header("x-kvmd-passwd"),
        authorization: header("authorization"),
    };
    let (response, gate) = {
        let mut device = shared.device.lock().unwrap();
        let (response, gated) = respond(&mut device, request);
        (response, gated.then(|| device.gate.clone()))
    };
    if let Some(gate) = gate {
        gate.arrived.notify_one();
        gate.release.notified().await;
    }
    response
}

async fn serve(shared: Arc<Shared>) -> (Url, Vec<u8>) {
    let certified = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
    let pem = certified.cert.pem().into_bytes();
    let key = certified.key_pair.serialize_pem().into_bytes();
    let tls = RustlsConfig::from_pem(pem.clone(), key).await.unwrap();
    let app = Router::new().fallback(handle).with_state(shared);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        axum_server::from_tcp_rustls(listener, tls)
            .unwrap()
            .serve(app.into_make_service())
            .await
            .unwrap();
    });
    (
        Url::parse(&format!("https://localhost:{port}/")).unwrap(),
        pem,
    )
}

struct Fixture {
    runtime: tokio::runtime::Runtime,
    endpoint: Url,
    ca: Vec<u8>,
    shared: Arc<Shared>,
    generations: AtomicU64,
}

impl Fixture {
    fn new() -> Self {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap();
        let shared = Arc::new(Shared {
            device: Mutex::new(FakeDevice::new(0)),
        });
        let (endpoint, ca) = runtime.block_on(serve(shared.clone()));
        Self {
            runtime,
            endpoint,
            ca,
            shared,
            generations: AtomicU64::new(0),
        }
    }

    /// Replaces the device with a fresh one. Devices built before the reset keep addressing the old generation and are refused.
    fn reset(&self) {
        let generation = self.generations.fetch_add(1, Ordering::SeqCst) + 1;
        *self.shared.device.lock().unwrap() = FakeDevice::new(generation);
    }

    fn with<T>(&self, action: impl FnOnce(&mut FakeDevice) -> T) -> T {
        action(&mut self.shared.device.lock().unwrap())
    }

    fn requests_to(&self, path: &str) -> Vec<Request> {
        self.with(|device| {
            device
                .requests
                .iter()
                .filter(|request| request.path == path)
                .cloned()
                .collect()
        })
    }

    fn config(&self, profile: DeviceProfile) -> Config {
        let mut endpoint = self.endpoint.clone();
        endpoint.set_path(&format!("/g{}/", self.with(|device| device.generation)));
        Config {
            endpoint,
            profile,
            trusted_ca_pem: Some(self.ca.clone()),
            insecure_tls: false,
            timeout: Duration::from_secs(10),
        }
    }

    fn device(&self, profile: DeviceProfile) -> Kvmd {
        Kvmd::new(self.config(profile)).unwrap()
    }

    /// Runs an operation under a deadline so a lost wake-up fails the case instead of hanging the suite.
    fn run<F: Future>(&self, future: F) -> F::Output {
        self.runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(30), future)
                .await
                .expect("operation exceeded its deadline")
        })
    }
}

/// The fixture shared by every case of the test that expands it; each expansion owns a separate server.
macro_rules! fixture {
    () => {{
        static FIXTURE: OnceLock<Fixture> = OnceLock::new();
        let fixture: &'static Fixture = FIXTURE.get_or_init(Fixture::new);
        fixture.reset();
        fixture
    }};
}

/// Networked cases are costlier than pure ones, so each test chooses its own count.
fn cases(count: u32) -> ProptestConfig {
    ProptestConfig {
        cases: count,
        // Integration tests have no library root for the default persistence file to sit beside.
        failure_persistence: None,
        ..ProptestConfig::default()
    }
}

fn rendered(error: &Error) -> String {
    format!("{error} {error:?}")
}

async fn send(device: &Kvmd, button: bool, name: &str, pressed: bool) -> kvm_core::Result<()> {
    if button {
        device.mouse_button(name, pressed).await
    } else {
        device.key(name, pressed).await
    }
}

proptest! {
    #![proptest_config(cases(48))]

    // Breaks if names are interpolated into the query without encoding, if the state is sent in another spelling, or if keys and buttons swap parameters.
    #[test]
    fn held_input_names_survive_query_encoding(
        button in any::<bool>(),
        name in any::<String>(),
        pressed in any::<bool>(),
    ) {
        let fx = fixture!();
        let device = fx.device(DeviceProfile::PiKvm);
        let outcome = fx.run(async {
            let first = send(&device, button, &name, pressed).await;
            let second = send(&device, button, &name, false).await;
            (first, second)
        });
        prop_assert!(outcome.0.is_ok() && outcome.1.is_ok());
        let (path, param) = if button {
            ("/api/hid/events/send_mouse_button", "button")
        } else {
            ("/api/hid/events/send_key", "key")
        };
        let sent = fx.requests_to(path);
        prop_assert_eq!(sent.len(), 2);
        let expected = |state: bool| {
            vec![
                (param.to_owned(), name.clone()),
                ("state".to_owned(), state.to_string()),
            ]
        };
        prop_assert_eq!(&sent[0].query, &expected(pressed));
        prop_assert_eq!(&sent[1].query, &expected(false));
        prop_assert!(sent.iter().all(|request| request.method == "POST" && request.form.is_empty()));
        prop_assert!(fx.with(|device| device.keys.is_empty() && device.buttons.is_empty()));
    }

    // Breaks if coordinates are clamped, rescaled, swapped or truncated, or if the firmware mouse mode is not read first.
    #[test]
    fn absolute_moves_pass_coordinates_through_unscaled(x in any::<i16>(), y in any::<i16>()) {
        let fx = fixture!();
        let device = fx.device(DeviceProfile::PiKvm);
        prop_assert!(fx.run(device.mouse_move(x, y)).is_ok());
        let moves = fx.requests_to("/api/hid/events/send_mouse_move");
        prop_assert_eq!(moves.len(), 1);
        prop_assert_eq!(
            &moves[0].query,
            &vec![
                ("to_x".to_owned(), x.to_string()),
                ("to_y".to_owned(), y.to_string()),
            ]
        );
        let parsed: Vec<i16> = moves[0].query.iter().map(|(_, value)| value.parse().unwrap()).collect();
        prop_assert_eq!(parsed, vec![x, y]);
        let order: Vec<String> = fx.with(|device| device.requests.iter().map(|request| request.path.clone()).collect());
        prop_assert_eq!(order, vec!["/api/hid", "/api/hid/events/send_mouse_move"]);
    }

    // Breaks if a move is sent while the firmware reports relative mode.
    #[test]
    fn relative_mode_never_receives_an_absolute_move(x in any::<i16>(), y in any::<i16>()) {
        let fx = fixture!();
        fx.with(|device| device.relative = true);
        let device = fx.device(DeviceProfile::PiKvm);
        let result = fx.run(device.mouse_move(x, y));
        prop_assert!(matches!(result, Err(Error::Unsupported(_))));
        prop_assert!(fx.requests_to("/api/hid/events/send_mouse_move").is_empty());
    }

    // Breaks if channels or images are not encoded, if the requested state is not part of the verification, or if an unmatched observation reports success.
    #[test]
    fn gpio_and_media_controls_encode_names_and_verify_against_state(
        channel in prop_oneof![Just("lamp".to_owned()), any::<String>()],
        image in prop_oneof![Just("disk.iso".to_owned()), any::<String>()],
        state in any::<bool>(),
    ) {
        let fx = fixture!();
        let device = fx.device(DeviceProfile::PiKvm);
        let verdict = |matching: bool| {
            if matching { ControlOutcome::ObservedMatching } else { ControlOutcome::Unverified }
        };

        let gpio = fx.run(device.control(Control::Gpio { channel: channel.clone(), state }, Confirmation::new())).unwrap();
        prop_assert_eq!(gpio, verdict(channel == "lamp" && state));
        let sent = fx.requests_to("/api/gpio/switch");
        prop_assert_eq!(sent.len(), 1);
        prop_assert_eq!(
            &sent[0].query,
            &vec![
                ("channel".to_owned(), channel),
                ("state".to_owned(), state.to_string()),
                ("wait".to_owned(), "true".to_owned()),
            ]
        );

        let select = fx.run(device.control(Control::MediaSelect { image: image.clone() }, Confirmation::new())).unwrap();
        prop_assert_eq!(select, verdict(image == "disk.iso"));
        let sent = fx.requests_to("/api/msd/set_params");
        prop_assert_eq!(sent.len(), 1);
        prop_assert_eq!(&sent[0].query, &vec![("image".to_owned(), image)]);

        let connect = fx.run(device.control(Control::MediaConnect { connected: state }, Confirmation::new())).unwrap();
        prop_assert_eq!(connect, verdict(state));
        let sent = fx.requests_to("/api/msd/set_connected");
        prop_assert_eq!(sent.len(), 1);
        prop_assert_eq!(&sent[0].query, &vec![("connected".to_owned(), state.to_string())]);
    }

    // Breaks if the inventory bound is off by one, if a port outside it is sent, or if the GLKVM channel report is accepted as proof.
    #[test]
    fn switch_ports_outside_the_inventory_are_never_sent(
        port in prop_oneof![0u32..SWITCH_PORTS + 4, any::<u32>()],
        gl in any::<bool>(),
    ) {
        let fx = fixture!();
        let profile = if gl { DeviceProfile::GlKvm } else { DeviceProfile::PiKvm };
        let device = fx.device(profile);
        let result = fx.run(device.control(Control::Switch { port }, Confirmation::new()));
        let sent = fx.requests_to("/api/switch/set_active");
        if port < SWITCH_PORTS {
            let outcome = result.unwrap();
            prop_assert_eq!(sent.len(), 1);
            prop_assert_eq!(&sent[0].query, &vec![("port".to_owned(), port.to_string())]);
            let expected = if gl { ControlOutcome::Unverified } else { ControlOutcome::ObservedMatching };
            prop_assert_eq!(outcome, expected);
        } else {
            prop_assert!(matches!(result, Err(Error::Unsupported(_))));
            prop_assert!(sent.is_empty());
        }
    }

    // Breaks if two actions share a wire spelling, or if a hard reset, which leaves no observable state change, is reported as verified.
    #[test]
    fn power_actions_have_distinct_encodings_and_honest_verification(
        action in proptest::sample::select(vec![
            PowerAction::On,
            PowerAction::Off,
            PowerAction::OffHard,
            PowerAction::ResetHard,
        ]),
    ) {
        let fx = fixture!();
        let device = fx.device(DeviceProfile::PiKvm);
        let (wire, expected) = match action {
            PowerAction::On => ("on", ControlOutcome::ObservedMatching),
            PowerAction::Off => ("off", ControlOutcome::ObservedMatching),
            PowerAction::OffHard => ("off_hard", ControlOutcome::ObservedMatching),
            PowerAction::ResetHard => ("reset_hard", ControlOutcome::Unverified),
        };
        let outcome = fx.run(device.control(Control::Power(action), Confirmation::new())).unwrap();
        prop_assert_eq!(outcome, expected);
        let sent = fx.requests_to("/api/atx/power");
        prop_assert_eq!(sent.len(), 1);
        prop_assert_eq!(
            &sent[0].query,
            &vec![
                ("action".to_owned(), wire.to_owned()),
                ("wait".to_owned(), "true".to_owned()),
            ]
        );
    }
}

proptest! {
    #![proptest_config(cases(48))]

    // Breaks if the form is built by string concatenation (an ampersand or equals sign in a credential changes the fields), if the code is not appended to the password, or if a credential reaches the URL.
    #[test]
    fn cookie_login_form_carries_exact_credentials_and_nothing_in_the_url(
        user in any::<String>(),
        password in any::<String>(),
        totp in proptest::option::of("[0-9]{6}"),
    ) {
        let fx = fixture!();
        let device = fx.device(DeviceProfile::PiKvm);
        let result = fx.run(device.login_with_totp(&user, &password, totp.as_deref()));
        prop_assert!(matches!(result, Ok(LoginState::Authenticated)));
        let sent = fx.requests_to("/api/auth/login");
        prop_assert_eq!(sent.len(), 1);
        prop_assert_eq!(sent[0].target.as_str(), "/api/auth/login");
        let expected = vec![
            ("user".to_owned(), user),
            ("passwd".to_owned(), format!("{password}{}", totp.unwrap_or_default())),
        ];
        prop_assert_eq!(&sent[0].form, &expected);
    }

    // The GLKVM login field holds 63 bytes including the code. Multi-byte characters make byte and character counts differ. Breaks if the limit counts characters, is off by one, or lets an over-long login reach the device.
    #[test]
    fn glkvm_password_and_code_are_limited_by_bytes(
        wide in 0usize..28,
        slack in -3i64..=3,
        totp in "[0-9]{6}",
    ) {
        let narrow = usize::try_from(57 - 2 * i64::try_from(wide).unwrap() + slack).unwrap();
        let password = format!("{}{}", "\u{e9}".repeat(wide), "a".repeat(narrow));
        prop_assert_eq!(password.len() + 6, usize::try_from(63 + slack).unwrap());
        let fx = fixture!();
        let device = fx.device(DeviceProfile::GlKvm);
        let result = fx.run(device.login_with_totp("user", &password, Some(&totp)));
        let sent = fx.requests_to("/api/auth/login");
        if slack > 0 {
            prop_assert!(matches!(result, Err(Error::InvalidResponse(_))));
            prop_assert!(sent.is_empty());
        } else {
            prop_assert!(matches!(result, Ok(LoginState::Authenticated)));
            prop_assert_eq!(sent.len(), 1);
        }
    }

    // A malformed code must fail before any request, and the failure must not depend on the code or password it rejected. Breaks if validation moves after the request or the message echoes either input.
    #[test]
    fn malformed_totp_fails_identically_before_any_request(
        totp in malformed_totp(),
        password in "SECRET-[a-z0-9]{1,8}",
        gl in any::<bool>(),
    ) {
        let fx = fixture!();
        let profile = if gl { DeviceProfile::GlKvm } else { DeviceProfile::PiKvm };
        let device = fx.device(profile);
        let baseline = fx
            .run(device.login_with_totp("user", "password", Some("x")))
            .err()
            .unwrap()
            .to_string();
        let result = fx.run(device.login_with_totp("user", &password, Some(&totp)));
        prop_assert!(matches!(&result, Err(Error::InvalidResponse(_))));
        prop_assert_eq!(result.err().unwrap().to_string(), baseline);
        prop_assert!(fx.with(|device| device.requests.is_empty()));
    }
}

/// Codes that are not exactly six ASCII digits, including near misses and wide digits that are six characters but eighteen bytes.
fn malformed_totp() -> impl Strategy<Value = String> {
    prop_oneof![
        any::<String>(),
        "[0-9]{0,5}",
        "[0-9]{7,9}",
        "[0-9]{5}[a-z]",
        "[\\x{FF10}-\\x{FF19}]{6}",
    ]
    .prop_filter("a valid code is not malformed", |code| {
        !(code.len() == 6 && code.bytes().all(|byte| byte.is_ascii_digit()))
    })
}

/// Whether a result is a pending approval that does (or does not) report an expiry. A helper because `prop_assert!` formats its condition, which cannot contain a struct pattern's braces.
fn approval_pending(result: &kvm_core::Result<LoginState>, with_expiry: bool) -> bool {
    matches!(result, Ok(LoginState::ApprovalPending { expires_in }) if expires_in.is_some() == with_expiry)
}

#[derive(Debug, Clone, Copy)]
enum Step {
    Accept,
    Reject,
    Pending,
    Approve,
    Complete,
    Check,
}

proptest! {
    #![proptest_config(cases(32))]

    // Models a GLKVM session over arbitrary login, approval and check sequences. A check carries the latest login's cookie only while the model is authenticated and carries none after any failed, rejected or pending login, whatever session came before. The device answers every check with a cookie the client must ignore, and no result may render a password, approval token or session cookie. Breaks if a failed login keeps the old client, if a pending approval keeps the old cookie, if a later response replaces the session, or if a result echoes device-supplied secrets.
    #[test]
    fn authentication_transitions_never_carry_a_previous_session(
        steps in vec(
            proptest::sample::select(vec![
                Step::Accept,
                Step::Reject,
                Step::Pending,
                Step::Approve,
                Step::Complete,
                Step::Check,
            ]),
            1..14,
        ),
    ) {
        let fx = fixture!();
        let device = fx.device(DeviceProfile::GlKvm);
        let mut authenticated = false;
        let mut pending = false;
        let mut approved = false;
        for step in steps {
            let text = match step {
                Step::Accept | Step::Reject | Step::Pending => {
                    fx.with(|device| {
                        device.login_mode = match step {
                            Step::Accept => LoginMode::Accept,
                            Step::Reject => LoginMode::Reject,
                            _ => LoginMode::Pending,
                        };
                    });
                    let result = fx.run(device.login("SECRET-user", "SECRET-password"));
                    match step {
                        Step::Accept => {
                            prop_assert!(matches!(result, Ok(LoginState::Authenticated)));
                            authenticated = true;
                            pending = false;
                        }
                        Step::Reject => {
                            prop_assert!(matches!(result, Err(Error::Authentication)));
                            authenticated = false;
                            pending = false;
                        }
                        _ => {
                            prop_assert!(approval_pending(&result, true));
                            authenticated = false;
                            pending = true;
                            approved = false;
                        }
                    }
                    format!("{result:?}")
                }
                Step::Approve => {
                    fx.with(|device| device.approved = true);
                    approved = true;
                    String::new()
                }
                Step::Complete => {
                    let result = fx.run(device.complete_approval());
                    if !pending {
                        prop_assert!(matches!(result, Err(Error::InvalidResponse(_))));
                    } else if !approved {
                        prop_assert!(approval_pending(&result, false));
                    } else {
                        prop_assert!(matches!(result, Ok(LoginState::Authenticated)));
                        authenticated = true;
                        pending = false;
                    }
                    format!("{result:?}")
                }
                Step::Check => {
                    let result = fx.run(device.check_auth());
                    prop_assert_eq!(result.is_ok(), authenticated);
                    if !authenticated {
                        prop_assert!(matches!(result, Err(Error::Authentication)));
                    }
                    let (path, cookie, latest) = fx.with(|device| {
                        let last = device.requests.last().unwrap();
                        (last.path.clone(), last.cookie.clone(), device.last_issued)
                    });
                    prop_assert_eq!(path.as_str(), "/api/auth/check");
                    if authenticated {
                        prop_assert_eq!(cookie, latest.map(|id| format!("auth_token=session-{id}")));
                    } else {
                        prop_assert_eq!(cookie, None);
                    }
                    format!("{result:?}")
                }
            };
            for marker in SECRET_MARKERS {
                prop_assert!(!text.contains(marker), "{text}");
            }
        }
    }
}

#[derive(Debug, Clone)]
enum Act {
    Press(bool, String),
    Release(bool, String),
    /// Starts a press or release and abandons the call once the device has the request.
    Cancel(bool, String, bool),
    ReleaseAll,
}

fn act() -> impl Strategy<Value = Act> {
    prop_oneof![
        (any::<bool>(), "[a-c]").prop_map(|(button, name)| Act::Press(button, name)),
        (any::<bool>(), "[a-c]").prop_map(|(button, name)| Act::Release(button, name)),
        (any::<bool>(), "[a-c]", any::<bool>())
            .prop_map(|(button, name, press)| Act::Cancel(button, name, press)),
        Just(Act::ReleaseAll),
    ]
}

proptest! {
    #![proptest_config(cases(24))]

    // The adapter's tracked inputs must equal what the device holds after every completed call, and an abandoned call must cost the adapter nothing beyond releasing everything it had tracked before the next input. Breaks if a release leaves a key tracked, if abandonment skips the cleanup, if cleanup runs after the following input, or if a release is sent for an input that was never held.
    #[test]
    fn tracked_inputs_match_what_the_device_holds(acts in vec(act(), 1..14)) {
        let fx = fixture!();
        let device = fx.device(DeviceProfile::PiKvm);
        let gate = fx.with(|device| device.gate.clone());
        let mut held: BTreeSet<(bool, String)> = BTreeSet::new();
        let mut cleanup_pending = false;
        for act in acts {
            if cleanup_pending {
                // A second abandoned call is gated on the device, so the cleanup owed by the first must finish before it starts.
                if matches!(act, Act::Cancel(..)) {
                    prop_assert!(fx.run(device.mouse_move(0, 0)).is_ok());
                }
                held.clear();
                cleanup_pending = false;
            }
            match act {
                Act::Press(button, name) => {
                    prop_assert!(fx.run(send(&device, button, &name, true)).is_ok());
                    held.insert((button, name));
                }
                Act::Release(button, name) => {
                    if held.remove(&(button, name.clone())) {
                        prop_assert!(fx.run(send(&device, button, &name, false)).is_ok());
                    }
                }
                Act::ReleaseAll => {
                    prop_assert!(fx.run(device.release_inputs()).is_ok());
                    held.clear();
                }
                Act::Cancel(button, name, press) => {
                    if press || held.contains(&(button, name.clone())) {
                        fx.with(|device| device.gate_next = true);
                        fx.run(async {
                            tokio::select! {
                                _ = send(&device, button, &name, press) => {}
                                () = gate.arrived.notified() => {}
                            }
                        });
                        gate.release.notify_one();
                        if press {
                            held.insert((button, name));
                        } else {
                            held.remove(&(button, name));
                        }
                        cleanup_pending = true;
                    }
                }
            }
            if !cleanup_pending {
                let (keys, buttons) = fx.with(|device| (device.keys.clone(), device.buttons.clone()));
                let names = |button: bool| -> BTreeSet<String> {
                    held.iter()
                        .filter(|(kind, _)| *kind == button)
                        .map(|(_, name)| name.clone())
                        .collect()
                };
                prop_assert_eq!(keys, names(false));
                prop_assert_eq!(buttons, names(true));
            }
        }
        prop_assert!(fx.run(device.release_inputs()).is_ok());
        let (keys, buttons, stray) = fx.with(|device| {
            (device.keys.clone(), device.buttons.clone(), device.stray_releases.clone())
        });
        prop_assert!(keys.is_empty() && buttons.is_empty(), "{keys:?} {buttons:?}");
        prop_assert!(stray.is_empty(), "{stray:?}");
    }
}

proptest! {
    #![proptest_config(cases(48))]

    // Header and Basic credentials must arrive exactly in their headers and nowhere in the URL, body or cookies, on every request the adapter makes. Breaks if a credential is added to the query, re-encoded, or sent through the other mechanism as well.
    #[test]
    fn header_credentials_never_reach_the_url_or_body(
        basic in any::<bool>(),
        user in "SECRET-[ -9;-~]{0,12}[a-z0-9]",
        password in "SECRET-[ -9;-~]{0,12}[a-z0-9]",
        name in "[a-z]{1,6}",
    ) {
        let fx = fixture!();
        let config = fx.config(DeviceProfile::PiKvm);
        let built = if basic {
            Kvmd::with_basic_auth(config, &user, &password)
        } else {
            Kvmd::with_header_auth(config, &user, &password)
        };
        let device = built.unwrap();
        let outcome = fx.run(async {
            device.mouse_move(1, 2).await?;
            device.key(&name, true).await?;
            device.key(&name, false).await
        });
        prop_assert!(outcome.is_ok());
        let requests = fx.with(|device| device.requests.clone());
        prop_assert_eq!(requests.len(), 4);
        prop_assert_eq!(requests[0].target.as_str(), "/api/hid");
        let expected_basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{user}:{password}"))
        );
        for request in &requests {
            if basic {
                prop_assert_eq!(request.authorization.as_deref(), Some(expected_basic.as_str()));
                prop_assert!(request.user.is_none() && request.passwd.is_none());
            } else {
                prop_assert_eq!(request.user.as_deref(), Some(user.as_str()));
                prop_assert_eq!(request.passwd.as_deref(), Some(password.as_str()));
                prop_assert!(request.authorization.is_none());
            }
            prop_assert!(!request.target.contains("SECRET-"), "{}", request.target);
            prop_assert!(request.form.is_empty() && request.cookie.is_none());
        }
    }

    // A device that fails every request while echoing the credentials it received must not make them appear in an error, and every status must keep its classification. Breaks if an error is built from a response body or request header.
    #[test]
    fn failing_devices_are_reported_without_echoing_credentials(
        code in proptest::sample::select(vec![401u16, 403, 404, 500, 502, 503]),
        user in "SECRET-[a-z]{1,8}",
        password in "SECRET-[a-z]{1,8}",
    ) {
        let fx = fixture!();
        fx.with(|device| device.fail_status = Some(code));
        let device = Kvmd::with_header_auth(fx.config(DeviceProfile::PiKvm), &user, &password).unwrap();
        let checked = fx.run(device.check_auth());
        let moved = fx.run(device.mouse_move(1, 2));
        for result in [checked, moved] {
            prop_assert!(result.is_err());
            if let Err(error) = result {
                let text = rendered(&error);
                for marker in SECRET_MARKERS {
                    prop_assert!(!text.contains(marker), "{text}");
                }
                if code == 401 || code == 403 {
                    prop_assert!(matches!(error, Error::Authentication));
                } else {
                    prop_assert!(matches!(error, Error::Http(actual) if actual == code));
                }
            }
        }
    }
}

fn offline_config() -> Config {
    Config {
        endpoint: Url::parse("https://device.example/").unwrap(),
        profile: DeviceProfile::PiKvm,
        trusted_ca_pem: None,
        insecure_tls: false,
        timeout: Duration::from_secs(5),
    }
}

proptest! {
    #![proptest_config(cases(64))]

    // Credentials a header or Basic scheme cannot carry are refused at construction, and the refusal must not repeat them. Breaks if a rejected secret is formatted into the error.
    #[test]
    fn unusable_credentials_are_refused_without_being_echoed(
        prefix in "SECRET-[a-z]{0,6}",
        invalid in proptest::sample::select(vec!["\n", "\r", "\0", "\u{7f}"]),
        suffix in "[a-z]{0,6}",
    ) {
        let header = Kvmd::with_header_auth(offline_config(), "user", &format!("{prefix}{invalid}{suffix}"));
        let basic = Kvmd::with_basic_auth(offline_config(), "user", &format!("{prefix}:{suffix}"));
        for result in [header, basic] {
            prop_assert!(result.is_err());
            if let Err(error) = result {
                prop_assert!(!rendered(&error).contains("SECRET-"));
            }
        }
    }
}
