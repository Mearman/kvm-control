//! KVMD adapter and continuous MJPEG transport.
//!
//! PiKVM's [stream client](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/web/share/js/kvm/stream_mjpeg.js) and [WebSocket handler](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/server.py), and GLKVM's [stream client](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/web/share/js/kvm/stream_mjpeg.js) and [WebSocket handler](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/server.py), use these routes. [uStreamer](https://github.com/pikvm/ustreamer/blob/11fd828ab00c01a39bd0843ef442ef0b6d3cac41/src/ustreamer/http/server.c) emits multipart JPEG with or without Content-Length.
//!
//! These sources do not establish compatibility with every firmware version. Only synthetic server tests have been run; no physical device has been verified. The video API supports MJPEG only and does not change encoder settings. JPEG pixel data is decoded under resource limits before delivery; original metadata is preserved but not validated and must be treated as untrusted by consumers. Video capabilities remain unknown until this connection receives a valid JPEG snapshot or live frame. A successful operation records support for that connection; failures do not prove a capability unsupported.

use std::{
    collections::BTreeSet,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

#[cfg(test)]
mod proptests;
mod video;
#[cfg(test)]
mod video_tests;

use async_trait::async_trait;
use kvm_core::{
    Capabilities, Capability, Confirmation, Control, ControlOutcome, Device, DeviceProfile,
    DeviceStatus, Error, MouseMode, PowerAction, Result, Snapshot,
};
use reqwest::{Client, Method, StatusCode};
use tokio::sync::{Mutex, RwLock};
use url::Url;

// Bound both dimensions and decoded pixels before allocating or decoding untrusted frames.
const MAX_IMAGE_DIMENSION: u32 = 8192;
const MAX_IMAGE_PIXELS: u64 = 32 * 1024 * 1024;
const MAX_DECODED_BYTES: u64 = MAX_IMAGE_PIXELS * 4;
// Bound variable decoder allocations as well as the output. Recheck the formula against zune-jpeg when Cargo.lock changes.
const MAX_JPEG_WORKING_BYTES: u64 = MAX_DECODED_BYTES * 2;
const MAX_JPEG_VALIDATIONS: usize = 2;

fn jpeg_frame_safe(bytes: &[u8], stripped: &mut Vec<u8>) -> bool {
    let mut pos = 2;
    let mut copied = 0;
    let mut frame = false;
    let mut scan = false;
    let mut in_entropy = false;
    while pos < bytes.len() {
        if bytes[pos] != 0xff {
            if !in_entropy {
                return false;
            }
            pos += 1;
            continue;
        }
        let marker_start = pos;
        while pos < bytes.len() && bytes[pos] == 0xff {
            pos += 1;
        }
        let Some(&marker) = bytes.get(pos) else {
            return false;
        };
        pos += 1;
        match marker {
            0x00 | 0xd0..=0xd7 if in_entropy => continue,
            0xd9 => {
                if !frame || !scan || pos != bytes.len() {
                    return false;
                }
                stripped.extend_from_slice(&bytes[copied..pos]);
                return true;
            }
            0xd8 | 0x00 | 0x01 | 0xd0..=0xd7 => return false,
            _ => {}
        }
        in_entropy = false;
        let Some(length_bytes) = bytes.get(pos..pos + 2) else {
            return false;
        };
        let length = usize::from(u16::from_be_bytes([length_bytes[0], length_bytes[1]]));
        if length < 2 || pos + length > bytes.len() {
            return false;
        }
        let segment = &bytes[pos + 2..pos + length];
        // zune-jpeg retains APP1, APP2 and APP13 metadata, including per-segment vectors. These segments do not affect its pixel decoding; retain the original JPEG for delivery.
        if matches!(marker, 0xe1 | 0xe2 | 0xed) {
            stripped.extend_from_slice(&bytes[copied..marker_start]);
            copied = pos + length;
        }
        if matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf) {
            if frame || scan || !matches!(marker, 0xc0..=0xc2) || segment.len() < 6 {
                return false;
            }
            let height = u64::from(u16::from_be_bytes([segment[1], segment[2]]));
            let width = u64::from(u16::from_be_bytes([segment[3], segment[4]]));
            let count = usize::from(segment[5]);
            if !matches!(count, 1 | 3 | 4)
                || segment.len() != 6 + count * 3
                || width == 0
                || height == 0
                || width > u64::from(MAX_IMAGE_DIMENSION)
                || height > u64::from(MAX_IMAGE_DIMENSION)
                || width * height > MAX_IMAGE_PIXELS
            {
                return false;
            }
            let mut area = 0_u64;
            let mut max_h = 1_u64;
            let mut max_v = 1_u64;
            for component in segment[6..].as_chunks::<3>().0 {
                let h = component[1] >> 4;
                let v = component[1] & 15;
                if !matches!(h, 1 | 2 | 4) || !(1..=4).contains(&v) {
                    return false;
                }
                area += u64::from(h) * u64::from(v);
                max_h = max_h.max(u64::from(h));
                max_v = max_v.max(u64::from(v));
            }
            // mcu.rs and mcu_prog.rs retain coefficient planes even for separate baseline scans. The non-interleaved block count bounds both paths.
            let block_columns = width.div_ceil(8).max(width.div_ceil(8 * max_h));
            let block_rows = height
                .div_ceil(8)
                .max(height.div_ceil(8 * max_v) * max_v / max_h);
            let coefficients = block_columns * block_rows * 64 * size_of::<i16>() as u64 * area;
            // components.rs, worker.rs and mcu_prog.rs allocate row, upsample, raw coefficient and scratch buffers. Bound every component by the largest stride, factors and resize case, including i16 storage and Vec growth.
            let stride = width.div_ceil(8 * max_h) * 8 * max_h;
            let row_buffers =
                count as u64 * stride * (4 * 8 + 2 * 4 + 4 * 16 + 16 * 2 * 8 + 4 * 8 * 2) * 2 * 2
                    + stride * 8 * 2;
            // The original, filtered input, image's input copy and temporary Vec growth are bounded by the compressed input length. APP1, APP2 and APP13 never reach the decoder.
            let input_and_metadata = bytes.len() as u64 * 8;
            let working = coefficients + row_buffers + width * height * 4 + input_and_metadata;
            if working > MAX_JPEG_WORKING_BYTES {
                return false;
            }
            frame = true;
        }
        if marker == 0xda {
            if !frame {
                return false;
            }
            scan = true;
            in_entropy = true;
        }
        pos += length;
    }
    false
}

async fn jpeg_permit() -> Result<Arc<tokio::sync::OwnedSemaphorePermit>> {
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let slots = SLOTS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_JPEG_VALIDATIONS)));
    Ok(Arc::new(slots.clone().acquire_owned().await.map_err(
        |_| Error::Transport("JPEG validation unavailable".into()),
    )?))
}

#[cfg(test)]
static VALIDATION_GATE: std::sync::Mutex<
    Option<(
        tokio::sync::oneshot::Sender<()>,
        std::sync::mpsc::Receiver<()>,
    )>,
> = std::sync::Mutex::new(None);

async fn validate_jpeg_async(
    bytes: Vec<u8>,
    permit: Arc<tokio::sync::OwnedSemaphorePermit>,
) -> Result<Vec<u8>> {
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        #[cfg(test)]
        if bytes
            .windows(b"validation-test-gate".len())
            .any(|window| window == b"validation-test-gate")
        {
            let gate = VALIDATION_GATE.lock().unwrap().take();
            if let Some((started, release)) = gate {
                started.send(()).unwrap();
                release.recv().unwrap();
            }
        }
        validate_jpeg(&bytes)?;
        Ok(bytes)
    })
    .await
    .map_err(|_| Error::Transport("JPEG validation task failed".into()))?
}

#[cfg(test)]
async fn run_jpeg_validation<T: Send + 'static>(
    slots: Arc<tokio::sync::Semaphore>,
    validate: impl FnOnce() -> Result<T> + Send + 'static,
) -> Result<T> {
    let permit = slots
        .acquire_owned()
        .await
        .map_err(|_| Error::Transport("JPEG validation unavailable".into()))?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        validate()
    })
    .await
    .map_err(|_| Error::Transport("JPEG validation task failed".into()))?
}

fn validate_jpeg(bytes: &[u8]) -> Result<Vec<u8>> {
    use image::ImageDecoder;
    let invalid = || Error::InvalidResponse("invalid or oversized JPEG image".into());
    let mut stripped = Vec::with_capacity(bytes.len());
    if !bytes.starts_with(&[0xff, 0xd8]) || !jpeg_frame_safe(bytes, &mut stripped) {
        return Err(invalid());
    }
    let mut decoder = image::codecs::jpeg::JpegDecoder::new(std::io::Cursor::new(&stripped))
        .map_err(|_| invalid())?;
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(MAX_IMAGE_DIMENSION);
    limits.max_image_height = Some(MAX_IMAGE_DIMENSION);
    limits.max_alloc = Some(MAX_DECODED_BYTES);
    decoder.set_limits(limits).map_err(|_| invalid())?;
    let size = usize::try_from(decoder.total_bytes()).map_err(|_| invalid())?;
    if size as u64 > MAX_DECODED_BYTES {
        return Err(invalid());
    }
    let mut pixels = vec![0; size];
    decoder.read_image(&mut pixels).map_err(|_| invalid())?;
    Ok(stripped)
}

fn request_error(context: &'static str, error: &reqwest::Error) -> Error {
    let kind = if error.is_timeout() {
        "timeout"
    } else if error.is_connect() {
        "connection failure"
    } else {
        "request failure"
    };
    Error::Transport(format!("{context}: {kind}"))
}

/// Connection parameters. Credentials are supplied separately and are never formatted for logs.
#[derive(Clone)]
pub struct Config {
    /// HTTPS device origin or reverse-proxy prefix; the adapter appends `api/`.
    pub endpoint: Url,
    pub profile: DeviceProfile,
    pub trusted_ca_pem: Option<Vec<u8>>,
    pub insecure_tls: bool,
    pub timeout: Duration,
}

/// Result of the device's explicit two-step login response.
#[derive(Debug, PartialEq, Eq)]
pub enum LoginState {
    Authenticated,
    ApprovalPending { expires_in: Option<u64> },
}

// Bounds memory consumed by untrusted JSON responses while allowing device inventory data.
const MAX_JSON_BYTES: usize = 4 * 1024 * 1024;

async fn bounded_json(mut response: reqwest::Response) -> Result<serde_json::Value> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_JSON_BYTES as u64)
    {
        return Err(Error::InvalidResponse(
            "JSON response exceeds the size limit".into(),
        ));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| Error::Transport("JSON response read failed".into()))?
    {
        if chunk.len() > MAX_JSON_BYTES - bytes.len() {
            return Err(Error::InvalidResponse(
                "JSON response exceeds the size limit".into(),
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes)
        .map_err(|_| Error::InvalidResponse("invalid JSON response".into()))
}

struct Session {
    client: Client,
    video: Client,
    cookies: Arc<SessionCookies>,
}

#[derive(Default)]
struct SessionCookies {
    jar: reqwest::cookie::Jar,
    sealed: AtomicBool,
}

impl reqwest::cookie::CookieStore for SessionCookies {
    fn set_cookies(
        &self,
        headers: &mut dyn Iterator<Item = &reqwest::header::HeaderValue>,
        url: &Url,
    ) {
        if !self.sealed.load(Ordering::Acquire) {
            reqwest::cookie::CookieStore::set_cookies(&self.jar, headers, url);
        }
    }

    fn cookies(&self, url: &Url) -> Option<reqwest::header::HeaderValue> {
        reqwest::cookie::CookieStore::cookies(&self.jar, url)
    }
}

/// A KVMD connection with an explicitly authenticated session.
///
/// Input requests already submitted continue after caller cancellation. Dropping a submitted key or button request before collecting its outcome is safe outside a runtime and releases tracked inputs before any later input, movement or explicit release runs, including one already waiting for the input lock, so cleanup never interleaves with later inputs: a request whose outcome was not yet delivered releases them before giving up the lock, and one whose outcome was delivered marks cleanup pending for the next lock holder and schedules it on the captured runtime. A completed request does not hold the input lock while its outcome waits to be collected, and a release of an input that cleanup has just released is not sent again. Ambiguous outcomes permanently render cleanup indeterminate because a server may apply a timed-out request later, and a request interrupted by runtime shutdown counts as ambiguous. An ambiguous release is not replayed automatically. Dropping the last owner attempts to release tracked inputs on the captured runtime; process or runtime termination provides no cleanup guarantee.
///
/// Cookies are accepted only from login or approval responses. Later `Set-Cookie` headers, including rotation and expiry updates, are ignored. The inspected [PiKVM](https://github.com/pikvm/kvmd/blob/fcb903eb3be70d8eee4aee99e5fcc1c743e3ccef/kvmd/apps/kvmd/api/auth.py) and [GLKVM](https://github.com/gl-inet/glkvm/blob/3e8dd23c4bd638a4650433664cc3b0f3b4d29395/kvmd/apps/kvmd/api/auth.py) revisions issue authentication cookies on login, and GLKVM also issues them on approval. Other firmware requiring cookie rotation is not supported. Server-side expiry or revocation can return an authentication error; call `login` again with fresh credentials and complete any approval. Consequential and input requests are never automatically replayed, and ambiguous outcomes require observation before a caller decides what to do.
pub struct Kvmd {
    client: Arc<RwLock<Session>>,
    config: Config,
    endpoint: Url,
    profile: DeviceProfile,
    inputs: Arc<Mutex<Inputs>>,
    auth_headers: reqwest::header::HeaderMap,
    approval_token: Mutex<Option<String>>,
    snapshot_proven: AtomicBool,
    video_proven: Arc<AtomicBool>,
    cleanup_on_drop: bool,
    input_runtime: Arc<std::sync::Mutex<Option<tokio::runtime::Handle>>>,
    // Set without the input lock by a dropped input future and consumed under the lock, so a lock holder after the drop releases tracked inputs before its own request.
    cleanup_pending: Arc<AtomicBool>,
    #[cfg(test)]
    stream_storage: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    snapshot_storage: Arc<tokio::sync::Semaphore>,
}

impl std::ops::Deref for Session {
    type Target = Client;
    fn deref(&self) -> &Client {
        &self.client
    }
}

#[derive(Default)]
struct Inputs {
    keys: BTreeSet<String>,
    buttons: BTreeSet<String>,
    uncertain_keys: BTreeSet<String>,
    uncertain_buttons: BTreeSet<String>,
    indeterminate: bool,
    // Inputs whose release cleanup for an abandoned request confirmed since the last key or button request, so that request does not resend one.
    cleaned: Vec<(Held, String)>,
}

/// An input that stays held until released: a keyboard key or a mouse button.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Held {
    Key,
    Button,
}

impl Held {
    fn path(self) -> &'static str {
        match self {
            Self::Key => "hid/events/send_key",
            Self::Button => "hid/events/send_mouse_button",
        }
    }

    fn param(self) -> &'static str {
        match self {
            Self::Key => "key",
            Self::Button => "button",
        }
    }
}

impl Inputs {
    fn held(&self, kind: Held) -> &BTreeSet<String> {
        match kind {
            Held::Key => &self.keys,
            Held::Button => &self.buttons,
        }
    }

    /// Records a press or release outcome; an unconfirmed outcome tracks a press as held and marks the input and the session indeterminate.
    fn record(&mut self, kind: Held, name: &str, pressed: bool, confirmed: bool) {
        let (held, uncertain) = match kind {
            Held::Key => (&mut self.keys, &mut self.uncertain_keys),
            Held::Button => (&mut self.buttons, &mut self.uncertain_buttons),
        };
        if pressed {
            held.insert(name.to_owned());
        } else {
            held.remove(name);
        }
        if !confirmed {
            uncertain.insert(name.to_owned());
            self.indeterminate = true;
        }
    }
}

/// Records a submitted held-input request as ambiguous unless `finish` runs, so a request whose task is dropped mid-flight (runtime shutdown) is neither forgotten nor left tracked for replay.
struct InFlight<'a> {
    inputs: &'a mut Inputs,
    kind: Held,
    name: &'a str,
    pressed: bool,
    finished: bool,
}

impl InFlight<'_> {
    fn finish(mut self, confirmed: bool) {
        self.inputs
            .record(self.kind, self.name, self.pressed, confirmed);
        self.finished = true;
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.inputs
                .record(self.kind, self.name, self.pressed, false);
        }
    }
}

/// Puts back a pending-cleanup flag taken by a release that does not finish, so the next lock holder still releases tracked inputs.
struct RestorePending<'a> {
    flag: &'a AtomicBool,
    owed: bool,
}

impl RestorePending<'_> {
    fn complete(mut self) {
        self.owed = false;
    }
}

impl Drop for RestorePending<'_> {
    fn drop(&mut self) {
        if self.owed {
            self.flag.store(true, Ordering::SeqCst);
        }
    }
}

/// Owns an input request's outcome channel for the caller. Dropped before collecting, it closes the channel so delivery and abandonment cannot race: an outcome not yet delivered makes the input task release tracked inputs itself before unlocking, and an outcome delivered after the task unlocked marks cleanup pending for the next lock holder and schedules it on the captured runtime. Needs no current runtime, so dropping outside one is safe.
struct Collect<'a> {
    device: &'a Kvmd,
    outcome: Option<tokio::sync::oneshot::Receiver<Result<()>>>,
}

impl Collect<'_> {
    async fn outcome(mut self) -> Result<()> {
        let outcome = self
            .outcome
            .as_mut()
            .expect("outcome is collected once")
            .await;
        self.outcome = None;
        outcome.map_err(|_| Error::AmbiguousInput("input task failed".into()))?
    }
}

impl Drop for Collect<'_> {
    fn drop(&mut self) {
        let Some(mut outcome) = self.outcome.take() else {
            return;
        };
        outcome.close();
        // Empty or closed: the task sees the failed delivery under the lock and cleans up itself, or it was dropped mid-request and recorded the request as ambiguous.
        if outcome.try_recv().is_err() {
            return;
        }
        // Set only after the task recorded this outcome, and the task never takes the flag after delivering it, so whichever lock holder takes the flag next releases this input.
        self.device.cleanup_pending.store(true, Ordering::SeqCst);
        if let Some(runtime) = self
            .device
            .input_runtime
            .lock()
            .expect("input runtime lock poisoned")
            .clone()
        {
            let worker = self.device.input_worker();
            runtime.spawn(async move {
                let mut inputs = worker.inputs.lock().await;
                worker.clean_pending(&mut inputs).await;
            });
        }
    }
}

impl Kvmd {
    async fn snapshot_with_storage(&self, slots: Arc<tokio::sync::Semaphore>) -> Result<Snapshot> {
        const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;
        let storage = slots
            .try_acquire_owned()
            .map_err(|_| Error::Transport("snapshot capacity exhausted".into()))?;
        let mut response = self.get("streamer/snapshot").await?;
        if response
            .content_length()
            .is_some_and(|n| n > MAX_SNAPSHOT_BYTES as u64)
        {
            return Err(Error::InvalidResponse(
                "snapshot exceeds the size limit".into(),
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|error| request_error("snapshot read", &error))?
        {
            if chunk.len() > MAX_SNAPSHOT_BYTES - bytes.len() {
                return Err(Error::InvalidResponse(
                    "snapshot exceeds the size limit".into(),
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let permit = jpeg_permit().await?;
        let task = tokio::spawn(async move {
            let _storage = storage;
            validate_jpeg_async(bytes, permit).await
        });
        let jpeg = task
            .await
            .map_err(|_| Error::Transport("JPEG validation task failed".into()))??;
        self.snapshot_proven.store(true, Ordering::Release);
        Ok(Snapshot { jpeg })
    }

    /// Builds a client without contacting the device. TLS verification is enabled unless explicitly disabled.
    pub fn new(config: Config) -> Result<Self> {
        if config.endpoint.scheme() != "https"
            || config.endpoint.host_str().is_none()
            || !config.endpoint.username().is_empty()
            || config.endpoint.password().is_some()
            || config.endpoint.query().is_some()
            || config.endpoint.fragment().is_some()
        {
            return Err(Error::InvalidResponse(
                "endpoint must be an HTTPS URL without credentials, query or fragment".into(),
            ));
        }
        let client = Self::build_client(&config)?;
        client.cookies.sealed.store(true, Ordering::Release);
        Ok(Self {
            client: Arc::new(RwLock::new(client)),
            endpoint: config.endpoint.clone(),
            profile: config.profile,
            config,
            inputs: Arc::new(Mutex::new(Inputs::default())),
            auth_headers: reqwest::header::HeaderMap::new(),
            approval_token: Mutex::new(None),
            snapshot_proven: AtomicBool::new(false),
            video_proven: Arc::new(AtomicBool::new(false)),
            cleanup_on_drop: true,
            input_runtime: Arc::new(std::sync::Mutex::new(None)),
            cleanup_pending: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            stream_storage: Arc::new(tokio::sync::Semaphore::new(MAX_JPEG_VALIDATIONS)),
            #[cfg(test)]
            snapshot_storage: Arc::new(tokio::sync::Semaphore::new(MAX_JPEG_VALIDATIONS)),
        })
    }

    fn build_client(config: &Config) -> Result<Session> {
        let cookies = Arc::new(SessionCookies::default());
        let build = |limited: bool| -> Result<Client> {
            let mut builder = Client::builder()
                .connect_timeout(config.timeout)
                .cookie_provider(cookies.clone())
                .http1_only()
                .retry(reqwest::retry::never())
                .redirect(reqwest::redirect::Policy::none())
                .danger_accept_invalid_certs(config.insecure_tls);
            if limited {
                builder = builder.timeout(config.timeout);
            }
            if let Some(pem) = &config.trusted_ca_pem {
                let ca = reqwest::Certificate::from_pem(pem)
                    .map_err(|_| Error::InvalidResponse("invalid CA certificate".into()))?;
                builder = builder.add_root_certificate(ca);
            }
            builder
                .build()
                .map_err(|_| Error::Transport("could not build HTTP client".into()))
        };
        Ok(Session {
            client: build(true)?,
            video: build(false)?,
            cookies,
        })
    }

    /// Builds a client with credentials sent in KVMD's authentication headers.
    pub fn with_header_auth(config: Config, username: &str, password: &str) -> Result<Self> {
        use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
        let mut device = Self::new(config)?;
        let mut headers = HeaderMap::new();
        for (name, value) in [("x-kvmd-user", username), ("x-kvmd-passwd", password)] {
            let mut value = HeaderValue::from_str(value)
                .map_err(|_| Error::InvalidResponse("invalid authentication header".into()))?;
            value.set_sensitive(true);
            headers.insert(HeaderName::from_static(name), value);
        }
        // Rebuilding here would lose the configured CA and TLS policy, so headers are held by the device instead.
        device.auth_headers = headers;
        Ok(device)
    }

    /// Builds a client using HTTP Basic authentication over the configured HTTPS connection.
    pub fn with_basic_auth(config: Config, username: &str, password: &str) -> Result<Self> {
        use base64::Engine;
        if username.contains(':') || password.contains(':') {
            return Err(Error::InvalidResponse(
                "Basic credentials cannot contain colons".into(),
            ));
        }
        let mut device = Self::new(config)?;
        let encoded =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let mut header = reqwest::header::HeaderValue::from_str(&format!("Basic {encoded}"))
            .map_err(|_| Error::InvalidResponse("invalid Basic credentials".into()))?;
        header.set_sensitive(true);
        device
            .auth_headers
            .insert(reqwest::header::AUTHORIZATION, header);
        Ok(device)
    }

    /// Checks whether the server accepts the configured authentication.
    pub async fn check_auth(&self) -> Result<()> {
        self.json("auth/check").await.map(|_| ())
    }

    /// Returns the explicitly selected device profile.
    pub fn profile(&self) -> DeviceProfile {
        self.profile
    }

    /// Authenticates using KVMD's cookie login. Rejection does not identify whether TOTP is required. Tracked inputs must first be released; an ambiguous release permits reauthentication but leaves the input session indeterminate.
    pub async fn login(&self, username: &str, password: &str) -> Result<LoginState> {
        self.login_with_totp(username, password, None).await
    }

    /// Authenticates with an optional current six-digit TOTP code, appended to the password for KVMD. A fresh code is required for each new login.
    pub async fn login_with_totp(
        &self,
        username: &str,
        password: &str,
        totp: Option<&str>,
    ) -> Result<LoginState> {
        let mut credential = zeroize::Zeroizing::new(password.to_owned());
        if let Some(code) = totp {
            if code.len() != 6 || !code.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(Error::InvalidResponse(
                    "TOTP must contain six ASCII digits".into(),
                ));
            }
            credential.push_str(code);
            if self.profile == DeviceProfile::GlKvm && credential.len() > 63 {
                return Err(Error::InvalidResponse(
                    "GLKVM login password and TOTP exceed 63 bytes".into(),
                ));
            }
        }
        if !self.auth_headers.is_empty() {
            return Err(Error::InvalidResponse(
                "cookie login cannot replace configured header credentials".into(),
            ));
        }
        let inputs = self.inputs.lock().await;
        if !inputs.keys.is_empty() || !inputs.buttons.is_empty() {
            return Err(Error::AmbiguousInput(
                "release tracked inputs before changing authentication".into(),
            ));
        }
        let mut pending = self.approval_token.lock().await;
        *pending = None;
        let mut active = self.client.write().await;
        let empty = Self::build_client(&self.config)?;
        empty.cookies.sealed.store(true, Ordering::Release);
        *active = empty;
        let candidate = Self::build_client(&self.config)?;
        let url = self.url("auth/login")?;
        let response = candidate
            .client
            .post(url)
            .form(&[("user", username), ("passwd", credential.as_str())])
            .send()
            .await
            .map_err(|error| request_error("login", &error))?;
        Self::check(response.status())?;
        let body = bounded_json(response).await?;
        if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(Error::Authentication);
        }
        let result = body
            .get("result")
            .ok_or_else(|| Error::InvalidResponse("missing login result".into()))?;
        if result
            .get("two_step_required")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            if self.profile != DeviceProfile::GlKvm {
                return Err(Error::InvalidResponse(
                    "two-step approval is not supported by the PiKVM profile".into(),
                ));
            }
            let token = result
                .get("two_step_token")
                .and_then(serde_json::Value::as_str)
                .filter(|s| !s.is_empty())
                .ok_or_else(|| Error::InvalidResponse("missing approval token".into()))?;
            let expires_in = result
                .get("expires_in")
                .and_then(serde_json::Value::as_u64)
                .ok_or_else(|| Error::InvalidResponse("missing approval expiry".into()))?;
            *pending = Some(token.to_owned());
            return Ok(LoginState::ApprovalPending {
                expires_in: Some(expires_in),
            });
        }
        candidate.cookies.sealed.store(true, Ordering::Release);
        self.verify_client(&candidate.client).await?;
        *active = candidate;
        Ok(LoginState::Authenticated)
    }

    /// Checks a pending GLKVM approval without exposing its temporary token.
    pub async fn complete_approval(&self) -> Result<LoginState> {
        if self.profile != DeviceProfile::GlKvm {
            return Err(Error::Unsupported(
                "device approval requires the GLKVM profile".into(),
            ));
        }
        let inputs = self.inputs.lock().await;
        if !inputs.keys.is_empty() || !inputs.buttons.is_empty() {
            return Err(Error::AmbiguousInput(
                "release tracked inputs before changing authentication".into(),
            ));
        }
        let mut pending = self.approval_token.lock().await;
        let token = pending
            .as_ref()
            .ok_or_else(|| Error::InvalidResponse("no approval is pending".into()))?;
        let mut active = self.client.write().await;
        let candidate = Self::build_client(&self.config)?;
        let response = candidate
            .client
            .post(self.url("auth/two_step_complete")?)
            .form(&[("two_step_token", token)])
            .send()
            .await
            .map_err(|error| request_error("approval", &error))?;
        Self::check(response.status())?;
        let body = bounded_json(response).await?;
        if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(Error::Authentication);
        }
        let result = body
            .get("result")
            .ok_or_else(|| Error::InvalidResponse("missing approval result".into()))?;
        if result.get("status").and_then(serde_json::Value::as_str) == Some("pending") {
            return Ok(LoginState::ApprovalPending { expires_in: None });
        }
        if result
            .get("token")
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(Error::InvalidResponse(
                "missing approved session token".into(),
            ));
        }
        candidate.cookies.sealed.store(true, Ordering::Release);
        self.verify_client(&candidate.client).await?;
        *active = candidate;
        *pending = None;
        Ok(LoginState::Authenticated)
    }

    async fn verify_client(&self, client: &Client) -> Result<()> {
        let response = client
            .get(self.url("auth/check")?)
            .send()
            .await
            .map_err(|error| request_error("authentication check", &error))?;
        Self::check(response.status())?;
        let body = bounded_json(response).await?;
        if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(Error::Authentication);
        }
        Ok(())
    }

    fn url(&self, path: &str) -> Result<Url> {
        let mut base = self.endpoint.clone();
        if !base.path().ends_with('/') {
            base.set_path(&format!("{}/", base.path()));
        }
        base.join(&format!("api/{path}"))
            .map_err(|_| Error::InvalidResponse("invalid endpoint".into()))
    }

    fn check(status: StatusCode) -> Result<()> {
        if status.is_success() {
            Ok(())
        } else if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            Err(Error::Authentication)
        } else {
            Err(Error::Http(status.as_u16()))
        }
    }

    async fn get(&self, path: &str) -> Result<reqwest::Response> {
        let active = self.client.read().await;
        self.get_with(&active.client, path).await
    }

    async fn get_with(&self, active: &Client, path: &str) -> Result<reqwest::Response> {
        let response = active
            .get(self.url(path)?)
            .headers(self.auth_headers.clone())
            .send()
            .await
            .map_err(|error| request_error("device request", &error))?;
        Self::check(response.status())?;
        Ok(response)
    }

    async fn json(&self, path: &str) -> Result<serde_json::Value> {
        let active = self.client.read().await;
        self.json_with(&active.client, path).await
    }

    async fn json_with(&self, active: &Client, path: &str) -> Result<serde_json::Value> {
        let value = bounded_json(self.get_with(active, path).await?).await?;
        if value.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(Error::InvalidResponse(
                "KVMD response was not successful".into(),
            ));
        }
        value
            .get("result")
            .cloned()
            .ok_or_else(|| Error::InvalidResponse("missing result".into()))
    }

    async fn discover_with(&self, active: &Client, path: &str) -> Result<Capability> {
        match self.json_with(active, path).await {
            Ok(state) => {
                let available = match path {
                    "gpio" => state
                        .pointer("/state/outputs")
                        .and_then(serde_json::Value::as_object)
                        .map(|outputs| !outputs.is_empty()),
                    "switch" => state
                        .pointer("/model/ports")
                        .and_then(serde_json::Value::as_array)
                        .map(|ports| !ports.is_empty()),
                    _ => state.get("enabled").and_then(serde_json::Value::as_bool),
                };
                Ok(match available {
                    Some(true) => Capability::Supported,
                    Some(false) => Capability::Unsupported,
                    None => Capability::Unknown,
                })
            }
            Err(Error::Http(404)) => Ok(Capability::Unsupported),
            Err(Error::Authentication) => Ok(Capability::Unknown),
            Err(error) => Err(error),
        }
    }

    fn input_worker(&self) -> Self {
        Self {
            client: self.client.clone(),
            config: self.config.clone(),
            endpoint: self.endpoint.clone(),
            profile: self.profile,
            inputs: self.inputs.clone(),
            auth_headers: self.auth_headers.clone(),
            approval_token: Mutex::new(None),
            snapshot_proven: AtomicBool::new(false),
            video_proven: self.video_proven.clone(),
            cleanup_on_drop: false,
            input_runtime: self.input_runtime.clone(),
            cleanup_pending: self.cleanup_pending.clone(),
            #[cfg(test)]
            stream_storage: self.stream_storage.clone(),
            #[cfg(test)]
            snapshot_storage: self.snapshot_storage.clone(),
        }
    }

    fn capture_input_runtime(&self) {
        *self
            .input_runtime
            .lock()
            .expect("input runtime lock poisoned") = Some(tokio::runtime::Handle::current());
    }

    /// Sends one press or release and records its outcome in `inputs`, the caller holding the input lock.
    async fn send_held(
        &self,
        inputs: &mut Inputs,
        kind: Held,
        name: &str,
        pressed: bool,
    ) -> Result<()> {
        let in_flight = InFlight {
            inputs,
            kind,
            name,
            pressed,
            finished: false,
        };
        let result = self
            .input(
                kind.path(),
                &[
                    (kind.param(), name.to_owned()),
                    ("state", pressed.to_string()),
                ],
            )
            .await;
        in_flight.finish(result.is_ok());
        result
    }

    /// Releases every tracked key, then every tracked button, the caller holding the input lock. A failed release is tracked as uncertain and never resent.
    /// Releases every tracked input, the caller holding the input lock, and returns the inputs whose release the device confirmed. Any pending cleanup is taken first, so a drop that marks cleanup pending while this runs stays pending for the next lock holder, and a release interrupted before it finishes (runtime shutdown) leaves the flag set.
    async fn release_owed(&self, inputs: &mut Inputs) -> (Result<()>, Vec<(Held, String)>) {
        let owed = self.cleanup_pending.swap(false, Ordering::SeqCst);
        let restore = RestorePending {
            flag: &self.cleanup_pending,
            owed,
        };
        let mut failed = false;
        let mut released = Vec::new();
        for kind in [Held::Key, Held::Button] {
            for name in inputs.held(kind).clone() {
                if self.send_held(inputs, kind, &name, false).await.is_ok() {
                    released.push((kind, name));
                } else {
                    failed = true;
                }
            }
        }
        restore.complete();
        let result = if failed || inputs.indeterminate {
            Err(Error::AmbiguousInput(
                "input session remains indeterminate after an ambiguous operation".into(),
            ))
        } else {
            Ok(())
        };
        (result, released)
    }

    /// Releases tracked inputs for an abandoned request, the caller holding the input lock, recording each confirmed release in `inputs.cleaned`.
    async fn clean_abandoned(&self, inputs: &mut Inputs) {
        let (result, released) = self.release_owed(inputs).await;
        inputs.cleaned.extend(released);
        if let Err(error) = result {
            eprintln!("Input cleanup uncertain: {error}");
        }
    }

    /// Runs cleanup marked pending by a dropped input future, the caller holding the input lock.
    async fn clean_pending(&self, inputs: &mut Inputs) {
        if self.cleanup_pending.load(Ordering::SeqCst) {
            self.clean_abandoned(inputs).await;
        }
    }

    /// Submits a press or release on a task that holds the input lock until the outcome is recorded and delivered, not until it is collected. A release of an input that cleanup for an abandoned request confirmed released since the previous key or button request is not sent again.
    async fn held_input(&self, kind: Held, name: &str, pressed: bool) -> Result<()> {
        let mut inputs = self.inputs.clone().lock_owned().await;
        self.capture_input_runtime();
        let worker = self.input_worker();
        let (result_tx, result_rx) = tokio::sync::oneshot::channel();
        let name = name.to_owned();
        let collect = Collect {
            device: self,
            outcome: Some(result_rx),
        };
        tokio::spawn(async move {
            worker.clean_pending(&mut inputs).await;
            let cleaned = std::mem::take(&mut inputs.cleaned);
            let result = if !pressed && cleaned.contains(&(kind, name.clone())) {
                Ok(())
            } else {
                worker.send_held(&mut inputs, kind, &name, pressed).await
            };
            if result_tx.send(result).is_err() {
                worker.clean_abandoned(&mut inputs).await;
            }
        });
        collect.outcome().await
    }

    async fn input(&self, path: &str, params: &[(&str, String)]) -> Result<()> {
        let active = self.client.read().await;
        self.input_with(&active.client, path, params).await
    }

    async fn input_with(
        &self,
        active: &Client,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<()> {
        let response = active
            .request(Method::POST, self.url(path)?)
            .headers(self.auth_headers.clone())
            .query(params)
            .send()
            .await
            .map_err(|_| Error::AmbiguousInput("request failed after submission".into()))?;
        Self::check(response.status()).map_err(|e| Error::AmbiguousInput(e.to_string()))?;
        let body = bounded_json(response)
            .await
            .map_err(|_| Error::AmbiguousInput("invalid or oversized input response".into()))?;
        if body.get("ok").and_then(serde_json::Value::as_bool) != Some(true) {
            return Err(Error::AmbiguousInput("device did not confirm input".into()));
        }
        Ok(())
    }
}

fn hid_capability(state: &serde_json::Value, device: &str) -> Capability {
    match state.get("enabled").and_then(serde_json::Value::as_bool) {
        Some(false) => Capability::Unsupported,
        Some(true)
            if state
                .get(device)
                .and_then(|value| value.get("online"))
                .and_then(serde_json::Value::as_bool)
                == Some(true) =>
        {
            Capability::Supported
        }
        _ => Capability::Unknown,
    }
}

fn mouse_mode(state: &serde_json::Value) -> MouseMode {
    match state
        .pointer("/mouse/absolute")
        .and_then(serde_json::Value::as_bool)
    {
        Some(true) => MouseMode::Absolute,
        Some(false) => MouseMode::Relative,
        None => MouseMode::Unknown,
    }
}

#[async_trait]
impl Device for Kvmd {
    async fn video(&self) -> Result<Box<dyn kvm_core::VideoStream>> {
        video::open(self).await
    }

    async fn status(&self) -> Result<DeviceStatus> {
        Ok(DeviceStatus {
            info: self.json("info").await?,
            hid: self.json("hid").await?,
            streamer: self.json("streamer").await?,
        })
    }

    async fn capabilities(&self) -> Result<Capabilities> {
        let active = self.client.read().await;
        let mut status = Capability::Supported;
        match self.json_with(&active.client, "info").await {
            Ok(_) => {}
            Err(Error::Authentication | Error::Http(404)) => status = Capability::Unknown,
            Err(error) => return Err(error),
        }
        let hid = self.json_with(&active.client, "hid").await?;
        match self.json_with(&active.client, "streamer").await {
            Ok(_) => {}
            Err(Error::Authentication | Error::Http(404)) => status = Capability::Unknown,
            Err(error) => return Err(error),
        }
        Ok(Capabilities {
            status,
            snapshot: if self.snapshot_proven.load(Ordering::Acquire) {
                Capability::Supported
            } else {
                Capability::Unknown
            },
            keyboard: hid_capability(&hid, "keyboard"),
            mouse: hid_capability(&hid, "mouse"),
            mouse_mode: mouse_mode(&hid),
            live_video: if self.video_proven.load(Ordering::Acquire) {
                Capability::Supported
            } else {
                Capability::Unknown
            },
            power: self.discover_with(&active.client, "atx").await?,
            switch: self.discover_with(&active.client, "switch").await?,
            gpio: self.discover_with(&active.client, "gpio").await?,
            media: self.discover_with(&active.client, "msd").await?,
        })
    }

    async fn snapshot(&self) -> Result<Snapshot> {
        #[cfg(not(test))]
        static STORAGE: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        #[cfg(not(test))]
        let slots = STORAGE
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_JPEG_VALIDATIONS)))
            .clone();
        #[cfg(test)]
        let slots = self.snapshot_storage.clone();
        self.snapshot_with_storage(slots).await
    }

    async fn key(&self, key: &str, pressed: bool) -> Result<()> {
        self.held_input(Held::Key, key, pressed).await
    }

    async fn mouse_button(&self, button: &str, pressed: bool) -> Result<()> {
        self.held_input(Held::Button, button, pressed).await
    }

    async fn mouse_move(&self, x: i16, y: i16) -> Result<()> {
        let inputs = self.inputs.clone().lock_owned().await;
        self.capture_input_runtime();
        let worker = self.input_worker();
        tokio::spawn(async move {
            let mut inputs = inputs;
            worker.clean_pending(&mut inputs).await;
            let active = worker.client.read().await;
            let state = worker.json_with(&active.client, "hid").await?;
            if hid_capability(&state, "mouse") != Capability::Supported
                || mouse_mode(&state) != MouseMode::Absolute
            {
                return Err(Error::Unsupported(
                    "absolute mouse movement requires an online absolute-mode mouse".into(),
                ));
            }
            // Marked before submission so a task dropped mid-request (runtime shutdown) leaves the session indeterminate.
            let indeterminate = inputs.indeterminate;
            inputs.indeterminate = true;
            let result = worker
                .input_with(
                    &active.client,
                    "hid/events/send_mouse_move",
                    &[("to_x", x.to_string()), ("to_y", y.to_string())],
                )
                .await;
            if result.is_ok() {
                inputs.indeterminate = indeterminate;
            }
            result
        })
        .await
        .map_err(|_| Error::AmbiguousInput("input task failed".into()))?
    }

    async fn control(
        &self,
        operation: Control,
        _confirmation: Confirmation,
    ) -> Result<ControlOutcome> {
        let observed = operation.clone();
        let (capability, path, params) = match operation {
            Control::Power(action) => (
                "atx",
                "atx/power",
                vec![
                    (
                        "action",
                        match action {
                            PowerAction::On => "on",
                            PowerAction::Off => "off",
                            PowerAction::OffHard => "off_hard",
                            PowerAction::ResetHard => "reset_hard",
                        }
                        .to_owned(),
                    ),
                    ("wait", "true".to_owned()),
                ],
            ),
            Control::Switch { port } => (
                "switch",
                "switch/set_active",
                vec![("port", port.to_string())],
            ),
            Control::Gpio { channel, state } => (
                "gpio",
                "gpio/switch",
                vec![
                    ("channel", channel),
                    ("state", state.to_string()),
                    ("wait", "true".to_owned()),
                ],
            ),
            Control::MediaConnect { connected } => (
                "msd",
                "msd/set_connected",
                vec![("connected", connected.to_string())],
            ),
            Control::MediaSelect { image } => ("msd", "msd/set_params", vec![("image", image)]),
        };
        let active = self.client.read().await;
        if let Control::Switch { port } = &observed {
            let state = self.json_with(&active.client, "switch").await?;
            let ports = state
                .pointer("/model/ports")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| Error::Unsupported("switch port inventory not confirmed".into()))?;
            let entry = usize::try_from(*port)
                .ok()
                .and_then(|index| ports.get(index));
            if !entry.is_some_and(|entry| {
                entry.get("unit").and_then(serde_json::Value::as_u64) == Some(u64::from(*port / 4))
                    && entry.get("channel").and_then(serde_json::Value::as_u64)
                        == Some(u64::from(*port % 4))
            }) {
                return Err(Error::Unsupported(
                    "switch port is not in the device inventory".into(),
                ));
            }
        } else if self.discover_with(&active.client, capability).await? != Capability::Supported {
            return Err(Error::Unsupported(format!(
                "{capability} capability not confirmed"
            )));
        }
        self.input_with(&active.client, path, &params)
            .await
            .map_err(|error| Error::AmbiguousControl(error.to_string()))?;
        let mut state = self
            .json_with(&active.client, capability)
            .await
            .map_err(|_| {
                Error::AmbiguousControl("request acknowledged but observation failed".into())
            })?;
        if let Control::Switch { port } = &observed
            && self.profile == DeviceProfile::PiKvm
        {
            // The PiKVM handler queues the switch; poll read-only state without replaying the request.
            for _ in 0..4 {
                if state
                    .pointer("/summary/synced")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
                    && state
                        .pointer("/summary/active_port")
                        .and_then(serde_json::Value::as_u64)
                        == Some(u64::from(*port))
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
                state = self
                    .json_with(&active.client, capability)
                    .await
                    .map_err(|_| {
                        Error::AmbiguousControl(
                            "request acknowledged but observation failed".into(),
                        )
                    })?;
            }
        }
        let verified = match observed {
            Control::Power(PowerAction::On) => {
                state
                    .pointer("/leds/power")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
            }
            Control::Power(PowerAction::Off | PowerAction::OffHard) => {
                state
                    .pointer("/leds/power")
                    .and_then(serde_json::Value::as_bool)
                    == Some(false)
            }
            Control::Power(PowerAction::ResetHard) => false,
            Control::Switch { port } => {
                // GL's reported channel does not prove that USB switching completed.
                self.profile == DeviceProfile::PiKvm
                    && state
                        .pointer("/summary/synced")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
                    && state
                        .pointer("/summary/active_port")
                        .and_then(serde_json::Value::as_u64)
                        == Some(u64::from(port))
            }
            Control::Gpio {
                channel,
                state: expected,
            } => state
                .pointer("/state/outputs")
                .and_then(|v| v.get(&channel))
                .is_some_and(|output| {
                    output.get("online").and_then(serde_json::Value::as_bool) == Some(true)
                        && output.get("busy").and_then(serde_json::Value::as_bool) == Some(false)
                        && output.get("state").and_then(serde_json::Value::as_bool)
                            == Some(expected)
                }),
            Control::MediaConnect { connected } => {
                state
                    .pointer("/drive/connected")
                    .and_then(serde_json::Value::as_bool)
                    == Some(connected)
            }
            Control::MediaSelect { image } => {
                state
                    .pointer("/drive/image/name")
                    .and_then(serde_json::Value::as_str)
                    == Some(image.as_str())
            }
        };
        Ok(if verified {
            ControlOutcome::ObservedMatching
        } else {
            ControlOutcome::Unverified
        })
    }

    async fn release_inputs(&self) -> Result<()> {
        let mut inputs = self.inputs.clone().lock_owned().await;
        self.capture_input_runtime();
        let worker = self.input_worker();
        tokio::spawn(async move { worker.release_owed(&mut inputs).await.0 })
            .await
            .map_err(|_| Error::AmbiguousInput("release task failed".into()))?
    }
}

impl Drop for Kvmd {
    fn drop(&mut self) {
        if self.cleanup_on_drop
            && let Some(runtime) = self
                .input_runtime
                .lock()
                .expect("input runtime lock poisoned")
                .clone()
        {
            let worker = self.input_worker();
            runtime.spawn(async move {
                if let Err(error) = worker.release_inputs().await {
                    eprintln!("Input cleanup uncertain: {error}");
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queued_image_reads_wait_for_a_permit() {
        let first = jpeg_permit().await.unwrap();
        let second = jpeg_permit().await.unwrap();
        let mut waiter = Box::pin(jpeg_permit());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), &mut waiter)
                .await
                .is_err()
        );
        drop(first);
        let third = tokio::time::timeout(std::time::Duration::from_secs(3), waiter)
            .await
            .unwrap()
            .unwrap();
        drop((second, third));
    }

    #[tokio::test]
    async fn cancelled_validation_holds_slot_until_blocking_work_finishes() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(run_jpeg_validation(slots.clone(), move || {
            started_tx.send(()).unwrap();
            release_rx.recv().unwrap();
            Ok(())
        }));
        started_rx.await.unwrap();
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(slots.clone().try_acquire_owned().is_err());
        release_tx.send(()).unwrap();
        let permit = tokio::time::timeout(std::time::Duration::from_secs(3), slots.acquire_owned())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
    }

    #[tokio::test]
    async fn validation_releases_slot_after_failure_and_panic() {
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        assert!(
            run_jpeg_validation(slots.clone(), || Err::<(), _>(Error::InvalidResponse(
                "test".into()
            )))
            .await
            .is_err()
        );
        assert!(
            run_jpeg_validation(slots.clone(), || -> Result<()> {
                panic!("synthetic panic")
            })
            .await
            .is_err()
        );
        assert!(slots.try_acquire_owned().is_ok());
    }

    #[test]
    fn progressive_jpeg_is_decoded_and_oversized_header_rejected() {
        use base64::Engine;
        // An 8x8 synthetic RGB image encoded by ImageMagick with progressive scans.
        let mut image = base64::engine::general_purpose::STANDARD.decode("/9j/4AAQSkZJRgABAQAAAQABAAD/2wBDAAMCAgICAgMCAgIDAwMDBAYEBAQEBAgGBgUGCQgKCgkICQkKDA8MCgsOCwkJDRENDg8QEBEQCgwSExIQEw8QEBD/2wBDAQMDAwQDBAgEBAgQCwkLEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBAQEBD/wgARCAAIAAgDAREAAhEBAxEB/8QAFAABAAAAAAAAAAAAAAAAAAAABf/EABQBAQAAAAAAAAAAAAAAAAAAAAb/2gAMAwEAAhADEAAAAT3I3//EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAQUCf//EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQMBAT8Bf//EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQIBAT8Bf//EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEABj8Cf//EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAT8hf//aAAwDAQACAAMAAAAQf//EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQMBAT8Qf//EABQRAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQIBAT8Qf//EABQQAQAAAAAAAAAAAAAAAAAAAAD/2gAIAQEAAT8Qf//Z").unwrap();
        assert!(validate_jpeg(&image).is_ok());
        let sof = image
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xc2])
            .unwrap();
        image[sof + 5..sof + 9].copy_from_slice(&[0x20, 0x00, 0x20, 0x00]);
        assert!(validate_jpeg(&image).is_err());
    }

    #[test]
    fn malformed_jpeg_markers_are_rejected() {
        let mut image = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut image)
            .encode(&[64, 128, 192], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let sof = image
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xc0])
            .unwrap();
        let length = usize::from(u16::from_be_bytes([image[sof + 2], image[sof + 3]])) + 2;
        let mut duplicate = image.clone();
        duplicate.splice(sof..sof, image[sof..sof + length].iter().copied());
        assert!(!jpeg_frame_safe(&duplicate, &mut Vec::new()));
        let mut bad_length = image.clone();
        bad_length[sof + 2..sof + 4].copy_from_slice(&[0xff, 0xff]);
        assert!(!jpeg_frame_safe(&bad_length, &mut Vec::new()));
        let mut zero_sampling = image.clone();
        zero_sampling[sof + 11] = 0;
        assert!(!jpeg_frame_safe(&zero_sampling, &mut Vec::new()));
        let mut trailing = image.clone();
        trailing.push(0);
        assert!(!jpeg_frame_safe(&trailing, &mut Vec::new()));
        assert!(!jpeg_frame_safe(&image[..sof + 4], &mut Vec::new()));
    }

    #[tokio::test]
    async fn validation_strips_repeated_metadata_without_changing_the_frame() {
        let mut original = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut original)
            .encode(&[64, 128, 192], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        let mut frame = original[..2].to_vec();
        let mut app2 = b"ICC_PROFILE\0".to_vec();
        app2.extend_from_slice(&[1, 1, 0]);
        for _ in 0..1024 {
            frame.extend_from_slice(&[0xff, 0xe2, 0, 17]);
            frame.extend_from_slice(&app2);
        }
        let mut xmp = b"http://ns.adobe.com/xmp/extension/\0".to_vec();
        xmp.extend_from_slice(&[b'a'; 32]);
        xmp.extend_from_slice(&1_u32.to_be_bytes());
        xmp.extend_from_slice(&0_u32.to_be_bytes());
        xmp.push(b'x');
        frame.extend_from_slice(&[0xff, 0xe1]);
        frame.extend_from_slice(&u16::try_from(xmp.len() + 2).unwrap().to_be_bytes());
        frame.extend_from_slice(&xmp);
        frame.extend_from_slice(&original[2..]);
        let mut stripped = Vec::new();
        assert!(jpeg_frame_safe(&frame, &mut stripped));
        assert_eq!(stripped, original);
        assert_eq!(validate_jpeg(&frame).unwrap(), original);
        assert_eq!(
            validate_jpeg_async(frame.clone(), jpeg_permit().await.unwrap())
                .await
                .unwrap(),
            frame
        );
    }

    #[test]
    fn progressive_1080p_is_accepted() {
        let mut image = Vec::new();
        let pixels = vec![64; 1920 * 1080 * 3];
        let mut encoder = jpeg_encoder::Encoder::new(&mut image, 75);
        encoder.set_progressive(true);
        encoder
            .encode(&pixels, 1920, 1080, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        assert!(validate_jpeg(&image).is_ok());
    }

    #[test]
    fn baseline_1080p_and_sampling_budget() {
        let mut image = Vec::new();
        let pixels = vec![64; 1920 * 1080 * 3];
        image::codecs::jpeg::JpegEncoder::new(&mut image)
            .encode(&pixels, 1920, 1080, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert!(validate_jpeg(&image).is_ok());
        let sof = image
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xc0])
            .unwrap();
        for i in 0..3 {
            image[sof + 11 + i * 3] = 0x55;
        }
        assert!(!jpeg_frame_safe(&image, &mut Vec::new()));
    }

    #[test]
    fn jpeg_validation_checks_decode_and_dimensions() {
        let mut valid = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut valid)
            .encode(&[42, 84, 126], 1, 1, image::ExtendedColorType::Rgb8)
            .unwrap();
        assert!(validate_jpeg(&valid).is_ok());
        assert!(validate_jpeg(&[0xff, 0xd8, 0xff, 0xd9]).is_err());
        let mut truncated = valid[..valid.len() / 2].to_vec();
        truncated.extend_from_slice(&[0xff, 0xd9]);
        assert!(validate_jpeg(&truncated).is_err());
        let mut bomb = valid.clone();
        let sof = bomb
            .windows(2)
            .position(|bytes| bytes == [0xff, 0xc0])
            .unwrap();
        bomb[sof + 5..sof + 9].copy_from_slice(&[0x7f, 0xff, 0x7f, 0xff]);
        assert!(validate_jpeg(&bomb).is_err());
        let mut gray = Vec::new();
        image::codecs::jpeg::JpegEncoder::new(&mut gray)
            .encode(&[42], 1, 1, image::ExtendedColorType::L8)
            .unwrap();
        assert!(validate_jpeg(&gray).is_ok());
    }

    #[tokio::test]
    async fn snapshot_rejects_marker_only_image() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\n\xff\xd8\xff\xd9").await.unwrap();
        });
        let endpoint = Url::parse(&format!("http://{address}/")).unwrap();
        let client = reqwest::Client::new();
        let device = Kvmd {
            client: Arc::new(RwLock::new(Session {
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
            cleanup_pending: Default::default(),
            stream_storage: Arc::new(tokio::sync::Semaphore::new(MAX_JPEG_VALIDATIONS)),
            snapshot_storage: Arc::new(tokio::sync::Semaphore::new(MAX_JPEG_VALIDATIONS)),
        };
        assert!(matches!(
            device.snapshot().await,
            Err(Error::InvalidResponse(_))
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn snapshot_capacity_is_held_by_active_response() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (started, started_rx) = tokio::sync::oneshot::channel();
        let (release, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0; 4096];
            assert!(socket.read(&mut request).await.unwrap() > 0);
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 4\r\n\r\n")
                .await
                .unwrap();
            started.send(()).unwrap();
            release_rx.await.unwrap();
            socket.write_all(b"\xff\xd8\xff\xd9").await.unwrap();
        });
        let endpoint = Url::parse(&format!("http://{address}/")).unwrap();
        let client = reqwest::Client::new();
        let device = Kvmd {
            client: Arc::new(RwLock::new(Session {
                client: client.clone(),
                video: client,
                cookies: Default::default(),
            })),
            config: config(endpoint.as_str()),
            endpoint,
            profile: DeviceProfile::PiKvm,
            inputs: Default::default(),
            auth_headers: Default::default(),
            approval_token: Default::default(),
            snapshot_proven: Default::default(),
            video_proven: Default::default(),
            cleanup_on_drop: false,
            input_runtime: Default::default(),
            cleanup_pending: Default::default(),
            stream_storage: Arc::new(tokio::sync::Semaphore::new(MAX_JPEG_VALIDATIONS)),
            snapshot_storage: Arc::new(tokio::sync::Semaphore::new(MAX_JPEG_VALIDATIONS)),
        };
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let first = device.snapshot_with_storage(slots.clone());
        tokio::pin!(first);
        tokio::select! { _ = &mut first => panic!("snapshot completed early"), _ = started_rx => {} }
        assert!(matches!(
            device.snapshot_with_storage(slots.clone()).await,
            Err(Error::Transport(_))
        ));
        release.send(()).unwrap();
        assert!(matches!(first.await, Err(Error::InvalidResponse(_))));
        assert_eq!(slots.available_permits(), 1);
        server.await.unwrap();
    }

    fn config(endpoint: &str) -> Config {
        Config {
            endpoint: Url::parse(endpoint).unwrap(),
            profile: DeviceProfile::PiKvm,
            trusted_ca_pem: None,
            insecure_tls: false,
            timeout: Duration::from_secs(5),
        }
    }

    #[test]
    fn mouse_modes_preserve_unknown() {
        assert_eq!(
            mouse_mode(&serde_json::json!({"mouse": {"absolute": true}})),
            MouseMode::Absolute
        );
        assert_eq!(
            mouse_mode(&serde_json::json!({"mouse": {"absolute": false}})),
            MouseMode::Relative
        );
        assert_eq!(
            mouse_mode(&serde_json::json!({"mouse": {}})),
            MouseMode::Unknown
        );
    }

    #[test]
    fn hid_capabilities_distinguish_disabled_offline_and_missing() {
        let online = serde_json::json!({"enabled": true, "keyboard": {"online": true, "outputs": {"available": [], "active": ""}}, "mouse": {"online": false}});
        assert_eq!(hid_capability(&online, "keyboard"), Capability::Supported);
        assert_eq!(hid_capability(&online, "mouse"), Capability::Unknown);
        let disabled = serde_json::json!({"enabled": false, "keyboard": {"online": true}});
        assert_eq!(
            hid_capability(&disabled, "keyboard"),
            Capability::Unsupported
        );
        assert_eq!(
            hid_capability(&serde_json::json!({"enabled": true}), "keyboard"),
            Capability::Unknown
        );
        assert_eq!(
            hid_capability(
                &serde_json::json!({"keyboard": {"online": true}}),
                "keyboard"
            ),
            Capability::Unknown
        );
    }

    #[test]
    fn paths_include_api_and_preserve_proxy_prefix() {
        let client = Kvmd::new(config("https://example.com/proxy")).unwrap();
        assert_eq!(
            client.url("auth/login").unwrap().as_str(),
            "https://example.com/proxy/api/auth/login"
        );
        assert_eq!(
            client.url("hid/events/send_key").unwrap().as_str(),
            "https://example.com/proxy/api/hid/events/send_key"
        );
        let client = Kvmd::new(config("https://example.com/")).unwrap();
        assert_eq!(
            client.url("info").unwrap().as_str(),
            "https://example.com/api/info"
        );
    }

    #[test]
    fn rejects_insecure_and_credential_bearing_endpoints() {
        for endpoint in [
            "http://example.com/",
            "https://user:password@example.com/",
            "https://example.com/?token=x",
            "https://example.com/#fragment",
        ] {
            assert!(Kvmd::new(config(endpoint)).is_err());
        }
    }

    #[test]
    fn every_http_status_preserves_authentication_and_error_classification() {
        for code in 100..=999 {
            let status = StatusCode::from_u16(code).unwrap();
            let result = Kvmd::check(status);
            if status.is_success() {
                assert!(result.is_ok(), "status {code}");
            } else if code == 401 || code == 403 {
                assert!(
                    matches!(result, Err(Error::Authentication)),
                    "status {code}"
                );
            } else {
                assert!(
                    matches!(result, Err(Error::Http(actual)) if actual == code),
                    "status {code}"
                );
            }
        }
    }

    #[test]
    fn hid_capability_is_only_supported_for_enabled_online_devices() {
        for enabled in [None, Some(false), Some(true)] {
            for online in [None, Some(false), Some(true)] {
                for device in ["keyboard", "mouse"] {
                    let mut state = serde_json::json!({});
                    if let Some(value) = enabled {
                        state["enabled"] = value.into();
                    }
                    if let Some(value) = online {
                        state[device] = serde_json::json!({"online": value});
                    }
                    let expected = match (enabled, online) {
                        (Some(false), _) => Capability::Unsupported,
                        (Some(true), Some(true)) => Capability::Supported,
                        _ => Capability::Unknown,
                    };
                    assert_eq!(hid_capability(&state, device), expected, "{state}");
                }
            }
        }
    }

    #[test]
    fn auth_statuses_are_distinguished() {
        assert!(matches!(
            Kvmd::check(StatusCode::UNAUTHORIZED),
            Err(Error::Authentication)
        ));
        assert!(matches!(
            Kvmd::check(StatusCode::FORBIDDEN),
            Err(Error::Authentication)
        ));
        assert!(matches!(
            Kvmd::check(StatusCode::NOT_FOUND),
            Err(Error::Http(404))
        ));
        assert!(Kvmd::check(StatusCode::OK).is_ok());
    }
}
